//! The PostgreSQL [`SqlDriver`] — the **second** concrete driver behind the swappable seam (DB0),
//! proving the design: a new backend is a new [`SqlDriver`] impl plus one dsn-scheme arm, with **no**
//! change to the Noeta surface, the query builder, the repository, or the `@sql` tier. Behind the
//! `ring-postgres` feature so a build that never opens a `postgres://` connection links no PG client.
//!
//! Two backend differences are absorbed **here**, so the layers above stay driver-agnostic:
//!   * **Placeholders.** The query builder and `@sql` emit `?` (the neutral placeholder); Postgres
//!     wants `$1, $2, …`. [`to_dollar_placeholders`] rewrites them, skipping any `?` inside a string
//!     literal or quoted identifier.
//!   * **Typed NULL + value binding.** Postgres binds through the typed `ToSql` protocol; [`PgVal`]
//!     adapts a neutral [`SqlValue`] onto it (a `Null` binds as an untyped SQL NULL, accepted for any
//!     column type).
//!
//! The synchronous `postgres::Client` (blocking, its own runtime) matches the sync `SqlDriver` trait
//! exactly like `rusqlite`. **TLS** is a pure-Rust rustls connector (ring provider, bundled Mozilla
//! roots). The dsn's `sslmode` ([`SslMode`], modeled after libpq) governs both *whether* TLS is used
//! and *whether the server certificate is authenticated*: `prefer` (the default) verifies against the
//! bundled roots and falls back to plaintext; `require` encrypts but does **not** verify the
//! certificate (libpq parity — see [`SslMode::Require`]); `verify-ca`/`verify-full` require verified
//! TLS. So a local server and a managed/hosted one both work from the same code.

use std::error::Error;
use std::str::FromStr;
use std::sync::Arc;

use bytes::BytesMut;
use postgres::Client;
use postgres::types::{IsNull, ToSql, Type, to_sql_checked};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};

use crate::driver::{Row, SqlDriver, SqlValue};

/// A PostgreSQL-backed [`SqlDriver`] over an owned blocking [`postgres::Client`]. Not cloneable —
/// which is why the extern value ([`crate::conn::ConnectionBox`]) shares it through an `Arc<Mutex<…>>`.
pub struct PostgresDriver {
    client: Client,
}

impl std::fmt::Debug for PostgresDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PostgresDriver")
    }
}

impl PostgresDriver {
    /// Connect to the server named by `dsn` (a libpq connection string / URL, e.g.
    /// `postgres://user:pass@host:5432/db?sslmode=require`). The dsn's `sslmode` ([`SslMode`]) selects
    /// the TLS behavior: whether TLS is negotiated (default `prefer` → try TLS, fall back to plaintext)
    /// and whether the server certificate is verified — so this connects to a plaintext local server, a
    /// verified managed one, or (with `sslmode=require`) an encrypted-but-unverified one from the same
    /// code. An unknown `sslmode` value is a clear error before any connection is attempted.
    pub fn connect(dsn: &str) -> Result<PostgresDriver, String> {
        let mode = ssl_mode_of(dsn)?;
        let client_dsn = dsn_for_client(dsn, mode);
        Client::connect(&client_dsn, make_tls(mode))
            .map(|client| PostgresDriver { client })
            .map_err(|e| e.to_string())
    }
}

/// How the Postgres driver treats TLS for a connection — the value parsed out of the dsn's `sslmode`
/// parameter, modeling libpq's SSL modes. Two independent security properties vary across the
/// variants: whether the connection **must** be encrypted, and whether the server's certificate is
/// **authenticated** (verified against a trust store). Encryption without authentication (see
/// [`SslMode::Require`]) stops passive eavesdropping but NOT an active man-in-the-middle who can
/// present any certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SslMode {
    /// `disable` — never negotiate TLS; the connection is always plaintext. No encryption and no
    /// authentication; use only over an already-trusted local socket.
    Disable,
    /// `prefer` (the **default**) — negotiate TLS when the server offers it and verify the server
    /// certificate against the bundled roots, otherwise fall back to a plaintext connection. The safe
    /// default: encrypted-and-authenticated when possible, never a hard failure against a plaintext
    /// local server.
    #[default]
    Prefer,
    /// `require` — TLS is **mandatory**, but the server certificate is **not verified**. The
    /// connection is encrypted (safe against passive eavesdropping) yet NOT authenticated, so it does
    /// not defend against an active man-in-the-middle who substitutes their own certificate. This is
    /// libpq's `sslmode=require`, deliberately distinct from `verify-ca`/`verify-full`: use it only
    /// where the network path to the server is already trusted (e.g. a private link) but the server
    /// presents a self-signed or otherwise unverifiable certificate.
    Require,
    /// `verify-ca` — TLS mandatory and the server certificate verified against the bundled roots.
    /// (This driver verifies the full certificate chain, so in practice it is at least as strict as
    /// libpq's CA-only check.)
    VerifyCa,
    /// `verify-full` — TLS mandatory and the server certificate fully verified (chain, validity, and
    /// hostname) against the bundled roots. The strongest mode.
    VerifyFull,
}

impl SslMode {
    /// Whether this mode authenticates the server certificate against the trust store. `false` only
    /// for [`SslMode::Require`] (encrypted-but-unauthenticated) and [`SslMode::Disable`] (no TLS).
    fn verifies_certificate(self) -> bool {
        matches!(
            self,
            SslMode::Prefer | SslMode::VerifyCa | SslMode::VerifyFull
        )
    }

    /// The `sslmode` token `tokio-postgres` itself understands (it parses only `disable`/`prefer`/
    /// `require`; the `verify-*` distinction is realized entirely by the certificate verifier this
    /// driver installs, so `verify-ca`/`verify-full` both require mandatory TLS at the transport —
    /// i.e. `require`).
    fn client_token(self) -> &'static str {
        match self {
            SslMode::Disable => "disable",
            SslMode::Prefer => "prefer",
            SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull => "require",
        }
    }
}

impl FromStr for SslMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "" | "prefer" => SslMode::Prefer,
            "disable" => SslMode::Disable,
            "require" => SslMode::Require,
            "verify-ca" => SslMode::VerifyCa,
            "verify-full" => SslMode::VerifyFull,
            other => {
                return Err(format!(
                    "para.db (postgres): unknown sslmode `{other}` (expected one of: disable, \
                     prefer, require, verify-ca, verify-full)"
                ));
            }
        })
    }
}

impl std::fmt::Display for SslMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SslMode::Disable => "disable",
            SslMode::Prefer => "prefer",
            SslMode::Require => "require",
            SslMode::VerifyCa => "verify-ca",
            SslMode::VerifyFull => "verify-full",
        })
    }
}

/// Parse the `sslmode` out of a libpq dsn's query string (`…?sslmode=…&…`), defaulting to
/// [`SslMode::Prefer`] when the parameter is absent. Case-insensitive on both the key and the value.
fn ssl_mode_of(dsn: &str) -> Result<SslMode, String> {
    let Some((_, query)) = dsn.split_once('?') else {
        return Ok(SslMode::default());
    };
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key.eq_ignore_ascii_case("sslmode") {
            return value.parse();
        }
    }
    Ok(SslMode::default())
}

/// The dsn to hand to `tokio-postgres`, whose parser understands only `disable`/`prefer`/`require`.
/// For `verify-ca`/`verify-full` the `sslmode` token is rewritten to `require` (mandatory TLS at the
/// transport); the certificate verification those modes ask for is provided by [`make_tls`]. Every
/// other dsn is passed through unchanged.
fn dsn_for_client(dsn: &str, mode: SslMode) -> String {
    match mode {
        SslMode::VerifyCa | SslMode::VerifyFull => rewrite_sslmode(dsn, mode.client_token()),
        SslMode::Disable | SslMode::Prefer | SslMode::Require => dsn.to_string(),
    }
}

/// Rewrite the value of the dsn's `sslmode` query parameter to `token`, preserving the connection's
/// base and every other parameter (and their order). Only called when the parameter is present.
fn rewrite_sslmode(dsn: &str, token: &str) -> String {
    let Some((base, query)) = dsn.split_once('?') else {
        return dsn.to_string();
    };
    let rewritten = query
        .split('&')
        .map(|pair| {
            let (key, _) = pair.split_once('=').unwrap_or((pair, ""));
            if key.eq_ignore_ascii_case("sslmode") {
                format!("{key}={token}")
            } else {
                pair.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("&");
    format!("{base}?{rewritten}")
}

/// Build the rustls TLS connector for `mode`. The `ring` crypto provider is used in every case (no
/// OpenSSL / C build). For an authenticating mode ([`SslMode::verifies_certificate`]) the server
/// certificate is checked against the bundled Mozilla root store (`webpki-roots`, so no system trust
/// store is required); for [`SslMode::Require`] a [`NoCertificateVerification`] verifier is installed
/// instead — it encrypts the connection but performs **no** certificate authentication (libpq
/// `sslmode=require` parity; see that variant's security note). No client certificate is presented.
fn make_tls(mode: SslMode) -> tokio_postgres_rustls::MakeRustlsConnect {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("ring provider supports the default protocol versions");
    let config = if mode.verifies_certificate() {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        builder.with_root_certificates(roots).with_no_client_auth()
    } else {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertificateVerification(provider)))
            .with_no_client_auth()
    };
    tokio_postgres_rustls::MakeRustlsConnect::new(config)
}

/// A rustls [`ServerCertVerifier`] that **accepts any server certificate** without checking its
/// chain, validity, or hostname. The transport is still encrypted — the handshake signature is
/// verified against the crypto provider's algorithms, so the peer proves possession of the presented
/// key — but the certificate is **not authenticated**, so an active man-in-the-middle presenting a
/// substitute certificate is not detected. Installed only for [`SslMode::Require`] (libpq
/// `sslmode=require`: encrypted, not authenticated); the `prefer`/`verify-*` modes verify against the
/// bundled roots instead.
#[derive(Debug)]
struct NoCertificateVerification(Arc<CryptoProvider>);

impl ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        // Deliberately unconditional: this is the encrypt-without-authenticate mode. The security
        // tradeoff is documented on the type and on `SslMode::Require`.
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

impl SqlDriver for PostgresDriver {
    fn execute(&mut self, sql: &str, params: &[SqlValue]) -> Result<i64, String> {
        let sql = to_dollar_placeholders(sql);
        let bound = to_pg(params);
        let refs: Vec<&(dyn ToSql + Sync)> =
            bound.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        self.client
            .execute(&sql, &refs)
            .map(|affected| affected as i64)
            .map_err(pg_err)
    }

    fn query(&mut self, sql: &str, params: &[SqlValue]) -> Result<Vec<Row>, String> {
        let sql = to_dollar_placeholders(sql);
        let bound = to_pg(params);
        let refs: Vec<&(dyn ToSql + Sync)> =
            bound.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        let rows = self.client.query(&sql, &refs).map_err(pg_err)?;

        let mut out: Vec<Row> = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut record: Row = Vec::with_capacity(row.len());
            for (i, col) in row.columns().iter().enumerate() {
                record.push((col.name().to_string(), value_of(row, i, col.type_())?));
            }
            out.push(record);
        }
        Ok(out)
    }

    fn execute_batch(&mut self, sql: &str) -> Result<(), String> {
        // `batch_execute` runs the whole script (multiple `;`-separated statements) over the simple
        // query protocol — no placeholder rewrite, so a migration body is executed verbatim in native
        // Postgres SQL, and `BEGIN`/`COMMIT`/`ROLLBACK` issue cleanly.
        self.client.batch_execute(sql).map_err(pg_err)
    }

    fn reset(&mut self) -> Result<(), String> {
        // The standard Postgres wipe: drop and recreate the `public` schema. This removes every table,
        // view, sequence, function, and (crucially) the `_noeta_migrations` tracking table in one step,
        // leaving an empty schema the runner then re-applies from zero. Targets `public` only; a project
        // using other schemas resets those itself.
        self.client
            .batch_execute("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
            .map_err(pg_err)
    }

    fn listen(&mut self, channel: &str) -> Result<(), String> {
        // `LISTEN` names an identifier, not a bind parameter, so the channel is quoted as one.
        self.client
            .batch_execute(&format!("LISTEN {}", quote_ident(channel)))
            .map_err(pg_err)
    }

    fn notify(&mut self, channel: &str) -> Result<(), String> {
        // Quoted identically to `listen`, so a `NOTIFY` matches a `LISTEN` on the same channel.
        self.client
            .batch_execute(&format!("NOTIFY {}", quote_ident(channel)))
            .map_err(pg_err)
    }

    fn notifications(&mut self) -> Result<Vec<String>, String> {
        use postgres::fallible_iterator::FallibleIterator;
        // A cheap round-trip processes any just-arrived wire bytes into the notification buffer; then
        // `try_iter` drains the buffered notifications non-blocking (it never waits on an empty queue).
        self.client.batch_execute("").map_err(pg_err)?;
        let mut notifications = self.client.notifications();
        let mut iter = notifications.iter();
        let mut channels = Vec::new();
        while let Some(n) = iter.next().map_err(pg_err)? {
            channels.push(n.channel().to_string());
        }
        Ok(channels)
    }
}

/// Quote a Postgres identifier (a `LISTEN`/`NOTIFY` channel name): wrap in double quotes and double any
/// embedded quote, so an arbitrary channel string can never break out of the identifier.
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Render a `postgres::Error` with its **server** detail — the bare `Display` is only `"db error"`,
/// so surface the DB error's message (the actual `syntax error at …` / `relation … does not exist`)
/// when there is one, else the transport error.
fn pg_err(e: postgres::Error) -> String {
    if let Some(db) = e.as_db_error() {
        format!("para.db (postgres): {}", db.message())
    } else {
        format!("para.db (postgres): {e}")
    }
}

/// Adapt the neutral params onto the `ToSql` protocol.
fn to_pg(params: &[SqlValue]) -> Vec<PgVal> {
    params
        .iter()
        .map(|p| match p {
            SqlValue::Int(n) => PgVal::Int(*n),
            SqlValue::Float(f) => PgVal::Float(*f),
            SqlValue::Text(s) => PgVal::Text(s.clone()),
            SqlValue::Bool(b) => PgVal::Bool(*b),
            SqlValue::Null => PgVal::Null,
        })
        .collect()
}

/// A neutral bind value projected onto Postgres's typed `ToSql`. It `accepts` any target type and
/// delegates the actual encoding to the inner Rust value (which validates the column type), so an
/// `Int`/`Float`/`Text`/`Bool` binds to a compatible column and a `Null` binds as an untyped SQL NULL
/// (`IsNull::Yes`) accepted for a column of any type — the one thing a fixed Rust `Option<T>` cannot do.
#[derive(Debug)]
enum PgVal {
    Int(i64),
    Float(f64),
    Text(String),
    Bool(bool),
    Null,
}

impl ToSql for PgVal {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn Error + Sync + Send>> {
        // Encode for the **target column type** `ty`, not the value's widest Rust type: Postgres binds
        // by the parameter type it inferred from the query, so an `Int` bound to an `int4` column must
        // encode as `i32` (4 bytes), not `i64` (8) — otherwise "incorrect binary data format". Route
        // each concrete value through its own `to_sql_checked`, which validates `ty` and gives a clean
        // error on a genuine mismatch (a value bound to an incompatible column).
        match self {
            PgVal::Null => Ok(IsNull::Yes),
            PgVal::Bool(b) => b.to_sql_checked(ty, out),
            PgVal::Text(s) => s.to_sql_checked(ty, out),
            PgVal::Int(n) => match *ty {
                Type::INT2 => i16::try_from(*n)?.to_sql_checked(ty, out),
                Type::INT4 => i32::try_from(*n)?.to_sql_checked(ty, out),
                Type::FLOAT4 => (*n as f32).to_sql_checked(ty, out),
                Type::FLOAT8 => (*n as f64).to_sql_checked(ty, out),
                _ => n.to_sql_checked(ty, out), // int8 and anything else i64 accepts
            },
            PgVal::Float(f) => match *ty {
                Type::FLOAT4 => (*f as f32).to_sql_checked(ty, out),
                _ => f.to_sql_checked(ty, out), // float8 and default
            },
        }
    }

    // Accept any target type: a `Null` fits any column, and each concrete value's own
    // `to_sql_checked` (above) reports a genuine mismatch. `accepts` is static (no `self`), so it
    // cannot discriminate per variant — the per-`ty` encoding in `to_sql` is where correctness lives.
    fn accepts(_ty: &Type) -> bool {
        true
    }

    to_sql_checked!();
}

/// Read column `i` (of Postgres type `ty`) out of a result row as a neutral [`SqlValue`]. A NULL in
/// any column reads back as [`SqlValue::Null`]. The scalar storage types map directly; any other type
/// is read best-effort as text (the row surface is textual in DB0), and a column that cannot even
/// render as text is a clear error rather than a panic.
fn value_of(row: &postgres::Row, i: usize, ty: &Type) -> Result<SqlValue, String> {
    let err = |e: postgres::Error| format!("para.db (postgres): reading column {i}: {e}");
    let value = match *ty {
        Type::INT8 => row.try_get::<_, Option<i64>>(i).map_err(err)?.map(SqlValue::Int),
        Type::INT4 => row
            .try_get::<_, Option<i32>>(i)
            .map_err(err)?
            .map(|n| SqlValue::Int(i64::from(n))),
        Type::INT2 => row
            .try_get::<_, Option<i16>>(i)
            .map_err(err)?
            .map(|n| SqlValue::Int(i64::from(n))),
        Type::FLOAT8 => row
            .try_get::<_, Option<f64>>(i)
            .map_err(err)?
            .map(SqlValue::Float),
        Type::FLOAT4 => row
            .try_get::<_, Option<f32>>(i)
            .map_err(err)?
            .map(|f| SqlValue::Float(f64::from(f))),
        Type::BOOL => row
            .try_get::<_, Option<bool>>(i)
            .map_err(err)?
            .map(SqlValue::Bool),
        _ => row
            .try_get::<_, Option<String>>(i)
            .map_err(|e| {
                format!(
                    "para.db (postgres): column {i} of type `{}` is not a scalar/text value DB0 can \
                     surface: {e}",
                    ty.name()
                )
            })?
            .map(SqlValue::Text),
    };
    Ok(value.unwrap_or(SqlValue::Null))
}

/// Rewrite the neutral `?` placeholders to Postgres's positional `$1, $2, …`. A `?` inside a
/// single-quoted string literal or a double-quoted identifier is left alone (it is data, not a
/// placeholder). Note: a literal Postgres `?`-family JSON operator (`?`, `?|`, `?&`) written by hand in
/// an `@sql` block would also be rewritten — the query builder and `@sql` only emit `?` as binds, so
/// this is safe for generated SQL; a hand-written jsonb existence operator needs care (a later slice).
fn to_dollar_placeholders(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len() + 8);
    let mut n: u32 = 0;
    let mut in_single = false;
    let mut in_double = false;
    for c in sql.chars() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                out.push(c);
            }
            '"' if !in_single => {
                in_double = !in_double;
                out.push(c);
            }
            '?' if !in_single && !in_double => {
                n += 1;
                out.push('$');
                out.push_str(&n.to_string());
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssl_mode_defaults_to_prefer_when_absent() {
        // Prefer is the safe default: verified TLS when offered, plaintext fallback otherwise.
        assert_eq!(SslMode::default(), SslMode::Prefer);
        assert_eq!(ssl_mode_of("postgres://u@h/db").unwrap(), SslMode::Prefer);
        assert_eq!(
            ssl_mode_of("postgres://u@h/db?connect_timeout=5").unwrap(),
            SslMode::Prefer
        );
    }

    #[test]
    fn ssl_mode_parses_every_libpq_mode_case_insensitively() {
        let cases = [
            ("disable", SslMode::Disable),
            ("prefer", SslMode::Prefer),
            ("require", SslMode::Require),
            ("verify-ca", SslMode::VerifyCa),
            ("VERIFY-FULL", SslMode::VerifyFull),
        ];
        for (token, expected) in cases {
            let dsn = format!("postgres://u@h/db?sslmode={token}");
            assert_eq!(ssl_mode_of(&dsn).unwrap(), expected, "sslmode={token}");
        }
        // The parameter key is matched case-insensitively too, and among other params.
        assert_eq!(
            ssl_mode_of("postgres://u@h/db?connect_timeout=5&SslMode=require").unwrap(),
            SslMode::Require
        );
    }

    #[test]
    fn an_unknown_ssl_mode_is_a_clear_error() {
        let err = ssl_mode_of("postgres://u@h/db?sslmode=insecure").unwrap_err();
        assert!(err.contains("unknown sslmode"), "{err}");
        assert!(err.contains("insecure"), "{err}");
    }

    #[test]
    fn only_require_and_disable_skip_certificate_verification() {
        assert!(!SslMode::Disable.verifies_certificate());
        assert!(SslMode::Prefer.verifies_certificate());
        assert!(!SslMode::Require.verifies_certificate());
        assert!(SslMode::VerifyCa.verifies_certificate());
        assert!(SslMode::VerifyFull.verifies_certificate());
    }

    #[test]
    fn client_token_maps_verify_modes_onto_require() {
        // tokio-postgres only understands disable/prefer/require; verify-* ride `require` transport
        // and get their verification from the installed verifier.
        assert_eq!(SslMode::Disable.client_token(), "disable");
        assert_eq!(SslMode::Prefer.client_token(), "prefer");
        assert_eq!(SslMode::Require.client_token(), "require");
        assert_eq!(SslMode::VerifyCa.client_token(), "require");
        assert_eq!(SslMode::VerifyFull.client_token(), "require");
    }

    #[test]
    fn ssl_mode_display_round_trips_through_parse() {
        for mode in [
            SslMode::Disable,
            SslMode::Prefer,
            SslMode::Require,
            SslMode::VerifyCa,
            SslMode::VerifyFull,
        ] {
            assert_eq!(mode.to_string().parse::<SslMode>().unwrap(), mode);
        }
    }

    #[test]
    fn verify_modes_rewrite_the_dsn_sslmode_to_require_for_the_client() {
        // verify-full → require for the transport, other params preserved in order.
        assert_eq!(
            dsn_for_client(
                "postgres://u:p@h:5432/db?sslmode=verify-full&connect_timeout=5",
                SslMode::VerifyFull
            ),
            "postgres://u:p@h:5432/db?sslmode=require&connect_timeout=5"
        );
        assert_eq!(
            dsn_for_client("postgres://h/db?sslmode=verify-ca", SslMode::VerifyCa),
            "postgres://h/db?sslmode=require"
        );
        // disable/prefer/require pass through untouched — tokio-postgres already understands them.
        assert_eq!(
            dsn_for_client("postgres://h/db?sslmode=require", SslMode::Require),
            "postgres://h/db?sslmode=require"
        );
        assert_eq!(
            dsn_for_client("postgres://h/db", SslMode::Prefer),
            "postgres://h/db"
        );
    }

    #[test]
    fn a_tls_connector_is_constructible_for_every_mode() {
        // Exercises verifier construction at the seam without a live server: the verifying path
        // builds the root store; the `require` path builds the no-verify verifier over the ring
        // provider. A panic here (e.g. an unsupported provider/protocol combination) would fail.
        for mode in [
            SslMode::Disable,
            SslMode::Prefer,
            SslMode::Require,
            SslMode::VerifyCa,
            SslMode::VerifyFull,
        ] {
            let _connector = make_tls(mode);
        }
    }

    #[test]
    fn placeholders_become_positional_dollars() {
        assert_eq!(
            to_dollar_placeholders("SELECT * FROM u WHERE a = ? AND b = ?"),
            "SELECT * FROM u WHERE a = $1 AND b = $2"
        );
    }

    #[test]
    fn a_question_mark_inside_a_string_or_identifier_is_left_alone() {
        assert_eq!(
            to_dollar_placeholders("SELECT '? not a bind' , \"we?rd\" WHERE x = ?"),
            "SELECT '? not a bind' , \"we?rd\" WHERE x = $1"
        );
    }

    #[test]
    fn no_placeholders_is_unchanged() {
        assert_eq!(
            to_dollar_placeholders("INSERT INTO t DEFAULT VALUES"),
            "INSERT INTO t DEFAULT VALUES"
        );
    }

    /// A full round-trip against a **live** PostgreSQL, run only when `NOETA_PG_TEST_DSN` is set (a CI
    /// service or a local container) so the unit suite stays hermetic. Exercises the whole driver end
    /// to end: the `?`→`$N` rewrite, typed binding of every `SqlValue` kind (int/float/text/bool/NULL),
    /// and reading each scalar column type back through the neutral surface.
    #[test]
    fn round_trip_against_a_live_server() {
        let Ok(dsn) = std::env::var("NOETA_PG_TEST_DSN") else {
            return; // no server configured — skip (the hermetic unit tests above still ran)
        };
        let mut d = PostgresDriver::connect(&dsn).expect("connect to NOETA_PG_TEST_DSN");
        d.execute("DROP TABLE IF EXISTS noeta_pg_it", &[]).unwrap();
        d.execute(
            "CREATE TABLE noeta_pg_it (id INT PRIMARY KEY, name TEXT, score DOUBLE PRECISION, \
             active BOOLEAN, note TEXT)",
            &[],
        )
        .unwrap();

        // INSERT with `?` placeholders and every value kind, including a NULL bound for `note`.
        let affected = d
            .execute(
                "INSERT INTO noeta_pg_it (id, name, score, active, note) VALUES (?, ?, ?, ?, ?)",
                &[
                    SqlValue::Int(1),
                    SqlValue::Text("Ada".into()),
                    SqlValue::Float(9.5),
                    SqlValue::Bool(true),
                    SqlValue::Null,
                ],
            )
            .unwrap();
        assert_eq!(affected, 1);

        // SELECT it back — a `?` bind on the WHERE, every column type mapped to its neutral value.
        let rows = d
            .query(
                "SELECT id, name, score, active, note FROM noeta_pg_it WHERE id = ?",
                &[SqlValue::Int(1)],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0],
            vec![
                ("id".to_string(), SqlValue::Int(1)),
                ("name".to_string(), SqlValue::Text("Ada".into())),
                ("score".to_string(), SqlValue::Float(9.5)),
                ("active".to_string(), SqlValue::Bool(true)),
                ("note".to_string(), SqlValue::Null),
            ]
        );
        d.execute("DROP TABLE noeta_pg_it", &[]).unwrap();
    }

    /// LISTEN/NOTIFY round-trip against a live server (env-gated). A listener connection subscribes to
    /// a channel; a *separate* writer connection fires `NOTIFY`; the listener's non-blocking poll then
    /// reports the channel — the basis of the reactive DB source (external writes → wake).
    #[test]
    fn listen_notify_round_trip() {
        let Ok(dsn) = std::env::var("NOETA_PG_TEST_DSN") else {
            return;
        };
        let mut listener = PostgresDriver::connect(&dsn).expect("listener");
        listener.listen("noeta_watch_test").expect("listen");
        assert!(
            listener.notifications().unwrap().is_empty(),
            "no notifications before any NOTIFY"
        );

        let mut writer = PostgresDriver::connect(&dsn).expect("writer");
        writer
            .execute("NOTIFY noeta_watch_test", &[])
            .expect("notify");

        // Delivery is asynchronous; poll a few times (non-blocking) until the notification lands.
        let mut seen = Vec::new();
        for _ in 0..40 {
            seen = listener.notifications().unwrap();
            if !seen.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert_eq!(seen, vec!["noeta_watch_test".to_string()]);
    }
}
