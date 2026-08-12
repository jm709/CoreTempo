#![expect(clippy::unwrap_used)]

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;

use coretempo_core::types::config::ServerOverrides;
use coretempo_core::types::id::Token;
use coretempo_core::workflow::{ConfigError, resolve_server, validate_workflow};

fn file() -> coretempo_core::types::config::WorkflowFile {
    validate_workflow(
        "[workflow]\nname = \"dev\"\nport = 5000\ndb = \"file.db\"\n\
         [server]\nlog = \"debug\"\n[agents.a]\ndir = \"/tmp\"\nprompt = \"p\"\n",
    )
    .unwrap()
}

#[test]
fn file_values_and_defaults_win_when_no_overrides() {
    let r = resolve_server(
        ServerOverrides::default(),
        ServerOverrides::default(),
        &file(),
    )
    .unwrap();
    assert_eq!(r.bind, IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_eq!(r.port, 5000);
    assert_eq!(r.db, PathBuf::from("file.db"));
    assert_eq!(r.log, "debug");
    assert_eq!(r.token.0.len(), 64); // generated on loopback
}

#[test]
fn env_beats_file_and_flags_beat_env() {
    let env = ServerOverrides {
        port: Some(6000),
        db: Some("env.db".into()),
        ..Default::default()
    };
    let r = resolve_server(ServerOverrides::default(), env.clone(), &file()).unwrap();
    assert_eq!(r.port, 6000);
    assert_eq!(r.db, PathBuf::from("env.db"));

    let flags = ServerOverrides {
        port: Some(7000),
        ..Default::default()
    };
    let r = resolve_server(flags, env, &file()).unwrap();
    assert_eq!(r.port, 7000);
    assert_eq!(r.db, PathBuf::from("env.db")); // flag absent → env layer wins
}

#[test]
fn provisioned_env_token_is_used_verbatim() {
    let tok = Token("ab".repeat(32));
    let env = ServerOverrides {
        token: Some(tok.clone()),
        ..Default::default()
    };
    let r = resolve_server(ServerOverrides::default(), env, &file()).unwrap();
    assert_eq!(r.token, tok);
}

#[test]
fn token_file_is_read_and_validated() {
    let dir = std::env::temp_dir().join(format!("coretempo-tok-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let good = dir.join("good");
    std::fs::write(&good, format!("{}\n", "cd".repeat(32))).unwrap();
    let flags = ServerOverrides {
        token_file: Some(good),
        ..Default::default()
    };
    let r = resolve_server(flags, ServerOverrides::default(), &file()).unwrap();
    assert_eq!(r.token.0, "cd".repeat(32));

    let bad = dir.join("bad");
    std::fs::write(&bad, "not-a-token").unwrap();
    let flags = ServerOverrides {
        token_file: Some(bad),
        ..Default::default()
    };
    let err = resolve_server(flags, ServerOverrides::default(), &file()).unwrap_err();
    assert!(matches!(err, ConfigError::BadTokenFile { .. }));
}

#[test]
fn non_loopback_bind_without_token_is_refused() {
    let flags = ServerOverrides {
        bind: Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
        ..Default::default()
    };
    let err = resolve_server(flags, ServerOverrides::default(), &file()).unwrap_err();
    assert!(matches!(err, ConfigError::NonLoopbackWithoutToken { .. }));
}

#[test]
fn non_loopback_bind_with_provisioned_token_is_allowed() {
    let flags = ServerOverrides {
        bind: Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
        ..Default::default()
    };
    let env = ServerOverrides {
        token: Some(Token("ef".repeat(32))),
        ..Default::default()
    };
    let r = resolve_server(flags, env, &file()).unwrap();
    assert_eq!(r.bind, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
}

/// Env vars are process-global: every `from_env` assertion lives in this single test.
#[test]
fn from_env_reads_coretempo_vars() {
    // SAFETY: only this test mutates CORETEMPO_* vars.
    unsafe {
        std::env::set_var("CORETEMPO_PORT", "4999");
        std::env::set_var("CORETEMPO_BIND", "127.0.0.1");
        std::env::set_var("CORETEMPO_DB", "/tmp/x.db");
        std::env::set_var("CORETEMPO_TOKEN", "ab".repeat(32));
        std::env::set_var("CORETEMPO_TOKEN_FILE", "/tmp/tokfile");
        std::env::set_var("CORETEMPO_LOG", "trace");
    }
    let o = ServerOverrides::from_env().unwrap();
    assert_eq!(o.port, Some(4999));
    assert_eq!(o.bind, Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    assert_eq!(o.db, Some(PathBuf::from("/tmp/x.db")));
    assert_eq!(o.token.unwrap().0, "ab".repeat(32));
    assert_eq!(o.token_file, Some(PathBuf::from("/tmp/tokfile")));
    assert_eq!(o.log.as_deref(), Some("trace"));

    unsafe { std::env::set_var("CORETEMPO_PORT", "not-a-port") };
    let err = ServerOverrides::from_env().unwrap_err();
    assert!(matches!(err, ConfigError::BadEnv { ref var, .. } if var == "CORETEMPO_PORT"));

    unsafe {
        for v in [
            "CORETEMPO_PORT",
            "CORETEMPO_BIND",
            "CORETEMPO_DB",
            "CORETEMPO_TOKEN",
            "CORETEMPO_TOKEN_FILE",
            "CORETEMPO_LOG",
        ] {
            std::env::remove_var(v);
        }
    }
}
