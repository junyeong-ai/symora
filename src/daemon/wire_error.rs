use serde::{Deserialize, Serialize};

use crate::error::{LspError, TimedOut, UnreadInput};
use crate::models::symbol::Language;

/// Serializable mirror of [`LspError`]. The daemon attaches one of these to
/// `RpcError.data` so the client can reconstruct the typed variant instead
/// of parsing the prose message back out.
///
/// Variants tagged in `kind` so the JSON stays self-describing on the wire.
/// `Io` and `Json` collapse to `Other` because their inner types aren't
/// portable across the socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireLspError {
    ServerStart {
        message: String,
    },
    NotConnected,
    ConnectionLost {
        method: String,
    },
    DaemonReplaced {
        method: String,
    },
    ServerNotInstalled {
        name: String,
        searched: Vec<String>,
        install_hint: String,
    },
    UnsupportedLanguage {
        language: String,
    },
    FeatureNotSupported {
        language: Language,
        server: String,
        feature: String,
        suggestion: String,
    },
    ServerTerminated {
        language: Language,
        #[serde(default)]
        established: bool,
    },
    Indexing {
        language: Language,
    },
    Timeout {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unread: Option<WireUnreadInput>,
    },
    RequestCancelled,
    ServerError {
        code: i32,
        message: String,
    },
    Protocol {
        message: String,
    },
    UnsupportedEdit {
        message: String,
    },
    FileTooLarge {
        path: String,
        size_mb: u64,
        limit_mb: u64,
    },
    FileNotText {
        path: String,
    },
    ServerNotConsulted,
    Other {
        message: String,
    },
}

/// [`UnreadInput`] on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireUnreadInput {
    language: Language,
    secs: u64,
}

impl From<&LspError> for WireLspError {
    fn from(err: &LspError) -> Self {
        match err {
            LspError::ServerStart(msg) => Self::ServerStart {
                message: msg.clone(),
            },
            LspError::NotConnected => Self::NotConnected,
            LspError::ConnectionLost(method) => Self::ConnectionLost {
                method: method.clone(),
            },
            LspError::DaemonReplaced(method) => Self::DaemonReplaced {
                method: method.clone(),
            },
            LspError::ServerNotInstalled {
                name,
                searched,
                install_hint,
            } => Self::ServerNotInstalled {
                name: name.clone(),
                searched: searched.clone(),
                install_hint: install_hint.clone(),
            },
            LspError::UnsupportedLanguage(lang) => Self::UnsupportedLanguage {
                language: lang.clone(),
            },
            LspError::FeatureNotSupported {
                language,
                server,
                feature,
                suggestion,
            } => Self::FeatureNotSupported {
                language: *language,
                server: server.clone(),
                feature: feature.clone(),
                suggestion: suggestion.clone(),
            },
            LspError::ServerTerminated {
                language,
                established,
            } => Self::ServerTerminated {
                language: *language,
                established: *established,
            },
            LspError::Indexing { language } => Self::Indexing {
                language: *language,
            },
            LspError::Timeout(timed_out) => Self::Timeout {
                message: timed_out.message.clone(),
                unread: timed_out.unread.map(|unread| WireUnreadInput {
                    language: unread.language,
                    secs: unread.secs,
                }),
            },
            LspError::RequestCancelled => Self::RequestCancelled,
            LspError::ServerError { code, message } => Self::ServerError {
                code: *code,
                message: message.clone(),
            },
            LspError::Protocol(msg) => Self::Protocol {
                message: msg.clone(),
            },
            LspError::UnsupportedEdit(msg) => Self::UnsupportedEdit {
                message: msg.clone(),
            },
            LspError::FileTooLarge {
                path,
                size_mb,
                limit_mb,
            } => Self::FileTooLarge {
                path: path.clone(),
                size_mb: *size_mb,
                limit_mb: *limit_mb,
            },
            LspError::FileNotText { path } => Self::FileNotText { path: path.clone() },
            LspError::ServerNotConsulted => Self::ServerNotConsulted,
            LspError::Io(e) => Self::Other {
                message: e.to_string(),
            },
            LspError::Json(e) => Self::Other {
                message: e.to_string(),
            },
        }
    }
}

impl From<WireLspError> for LspError {
    fn from(wire: WireLspError) -> Self {
        match wire {
            WireLspError::ServerStart { message } => LspError::ServerStart(message),
            WireLspError::NotConnected => LspError::NotConnected,
            WireLspError::ConnectionLost { method } => LspError::ConnectionLost(method),
            WireLspError::DaemonReplaced { method } => LspError::DaemonReplaced(method),
            WireLspError::ServerNotInstalled {
                name,
                searched,
                install_hint,
            } => LspError::ServerNotInstalled {
                name,
                searched,
                install_hint,
            },
            WireLspError::UnsupportedLanguage { language } => {
                LspError::UnsupportedLanguage(language)
            }
            WireLspError::FeatureNotSupported {
                language,
                server,
                feature,
                suggestion,
            } => LspError::FeatureNotSupported {
                language,
                server,
                feature,
                suggestion,
            },
            WireLspError::ServerTerminated {
                language,
                established,
            } => LspError::ServerTerminated {
                language,
                established,
            },
            WireLspError::Indexing { language } => LspError::Indexing { language },
            WireLspError::Timeout { message, unread } => LspError::Timeout(TimedOut {
                message,
                unread: unread.map(|unread| UnreadInput {
                    language: unread.language,
                    secs: unread.secs,
                }),
            }),
            WireLspError::RequestCancelled => LspError::RequestCancelled,
            WireLspError::ServerError { code, message } => LspError::ServerError { code, message },
            WireLspError::Protocol { message } => LspError::Protocol(message),
            WireLspError::UnsupportedEdit { message } => LspError::UnsupportedEdit(message),
            WireLspError::FileTooLarge {
                path,
                size_mb,
                limit_mb,
            } => LspError::FileTooLarge {
                path,
                size_mb,
                limit_mb,
            },
            WireLspError::FileNotText { path } => LspError::FileNotText { path },
            WireLspError::ServerNotConsulted => LspError::ServerNotConsulted,
            WireLspError::Other { message } => LspError::Protocol(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(err: LspError) -> LspError {
        let wire = WireLspError::from(&err);
        let json = serde_json::to_value(&wire).expect("serializable");
        let parsed: WireLspError = serde_json::from_value(json).expect("deserializable");
        parsed.into()
    }

    #[test]
    fn server_not_installed_round_trips_with_install_hint() {
        let recovered = round_trip(LspError::ServerNotInstalled {
            name: "rust-analyzer".into(),
            searched: vec!["/usr/bin".into()],
            install_hint: "rustup component add rust-analyzer".into(),
        });
        match recovered {
            LspError::ServerNotInstalled {
                name,
                searched,
                install_hint,
            } => {
                assert_eq!(name, "rust-analyzer");
                assert_eq!(searched, ["/usr/bin"]);
                assert_eq!(install_hint, "rustup component add rust-analyzer");
            }
            other => panic!("expected ServerNotInstalled, got {other:?}"),
        }
    }

    #[test]
    fn feature_not_supported_round_trips_with_language() {
        let recovered = round_trip(LspError::FeatureNotSupported {
            language: Language::Python,
            server: "pyright".into(),
            feature: "callHierarchy".into(),
            suggestion: "Use refs".into(),
        });
        match recovered {
            LspError::FeatureNotSupported {
                language,
                server,
                feature,
                ..
            } => {
                assert_eq!(language, Language::Python);
                assert_eq!(server, "pyright");
                assert_eq!(feature, "callHierarchy");
            }
            other => panic!("expected FeatureNotSupported, got {other:?}"),
        }
    }

    /// An edit the server produced but symora does not apply is a capability
    /// statement about this tool, not a protocol failure — it must cross the
    /// socket as itself so both modes classify it as unsupported.
    #[test]
    fn unsupported_edit_round_trips_as_itself() {
        let recovered = round_trip(LspError::UnsupportedEdit(
            "Rename requires a file rename operation".into(),
        ));
        match recovered {
            LspError::UnsupportedEdit(message) => {
                assert!(message.contains("file rename operation"));
            }
            other => panic!("expected UnsupportedEdit, got {other:?}"),
        }
    }

    /// The two terminations carry opposite advice — retry, or stop retrying
    /// and take a route that needs no server — so the fact that tells them
    /// apart has to survive the socket, not just the language.
    #[test]
    fn server_terminated_round_trips_whether_the_session_ever_existed() {
        for established in [true, false] {
            let recovered = round_trip(LspError::ServerTerminated {
                language: Language::Rust,
                established,
            });
            match recovered {
                LspError::ServerTerminated {
                    language,
                    established: recovered,
                } => {
                    assert_eq!(language, Language::Rust);
                    assert_eq!(recovered, established);
                }
                other => panic!("expected ServerTerminated, got {other:?}"),
            }
        }
    }

    /// A refusal for having begun on another daemon is made by the daemon
    /// that replaced it, so it has to cross the socket as itself to read as
    /// the command's lost daemon rather than a protocol failure.
    #[test]
    fn daemon_replaced_round_trips_as_itself() {
        match round_trip(LspError::DaemonReplaced("find_references".into())) {
            LspError::DaemonReplaced(method) => assert_eq!(method, "find_references"),
            other => panic!("expected DaemonReplaced, got {other:?}"),
        }
    }

    /// The cold-session "still indexing" answer must survive the socket
    /// typed — daemon and direct execution map it to the same structured
    /// error (invariant: the two modes never diverge in meaning).
    #[test]
    fn indexing_round_trips_language() {
        let recovered = round_trip(LspError::Indexing {
            language: Language::Rust,
        });
        match recovered {
            LspError::Indexing { language } => assert_eq!(language, Language::Rust),
            other => panic!("expected Indexing, got {other:?}"),
        }
    }

    /// What the daemon saw of a server that stopped reading reaches the
    /// client, which advises from it as a direct run would.
    #[test]
    fn timeout_round_trips_the_input_its_server_left_unread() {
        for unread in [
            None,
            Some(UnreadInput {
                language: Language::Python,
                secs: 61,
            }),
        ] {
            let recovered = round_trip(LspError::Timeout(TimedOut {
                message: "ran out".to_string(),
                unread,
            }));
            match recovered {
                LspError::Timeout(timed_out) => {
                    assert_eq!(timed_out.message, "ran out");
                    assert_eq!(timed_out.unread, unread);
                }
                other => panic!("expected Timeout, got {other:?}"),
            }
        }
    }

    #[test]
    fn io_collapses_to_protocol_with_message_preserved() {
        let recovered = round_trip(LspError::Io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "child stdin closed",
        )));
        match recovered {
            LspError::Protocol(msg) => assert!(msg.contains("child stdin closed")),
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    #[test]
    fn json_collapses_to_protocol_with_message_preserved() {
        let json_err: serde_json::Error =
            serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        let recovered = round_trip(LspError::Json(json_err));
        match recovered {
            LspError::Protocol(msg) => assert!(!msg.is_empty()),
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    /// A non-text file reaches the caller as itself rather than collapsing to
    /// `Other` — which the wire maps back to `Protocol`, and the CLI to an
    /// internal error — so the daemon and a direct run classify it the same.
    #[test]
    fn file_not_text_round_trips_as_itself() {
        let wire = WireLspError::from(&LspError::FileNotText {
            path: "/tmp/a.png".to_string(),
        });
        match LspError::from(wire) {
            LspError::FileNotText { path } => assert_eq!(path, "/tmp/a.png"),
            other => panic!("did not round trip: {other:?}"),
        }
    }

    /// A confined run's refusal must arrive as itself: collapsed to `Other` it
    /// maps back to a protocol failure, and the daemon would answer a question
    /// the direct run refused.
    #[test]
    fn server_not_consulted_round_trips_as_itself() {
        let wire = WireLspError::from(&LspError::ServerNotConsulted);
        assert!(matches!(LspError::from(wire), LspError::ServerNotConsulted));
    }

    #[test]
    fn json_shape_uses_kind_discriminator() {
        let wire = WireLspError::from(&LspError::ServerNotInstalled {
            name: "rust-analyzer".into(),
            searched: Vec::new(),
            install_hint: "x".into(),
        });
        let json = serde_json::to_value(&wire).unwrap();
        assert_eq!(json["kind"], "server_not_installed");
        assert_eq!(json["name"], "rust-analyzer");
    }
}
