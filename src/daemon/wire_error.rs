use serde::{Deserialize, Serialize};

use crate::error::LspError;
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
    ServerNotInstalled {
        name: String,
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
    },
    Timeout {
        message: String,
    },
    RequestCancelled,
    ServerError {
        code: i32,
        message: String,
    },
    Protocol {
        message: String,
    },
    FileTooLarge {
        path: String,
        size_mb: u64,
        limit_mb: u64,
    },
    Other {
        message: String,
    },
}

impl From<&LspError> for WireLspError {
    fn from(err: &LspError) -> Self {
        match err {
            LspError::ServerStart(msg) => Self::ServerStart {
                message: msg.clone(),
            },
            LspError::NotConnected => Self::NotConnected,
            LspError::ServerNotInstalled { name, install_hint } => Self::ServerNotInstalled {
                name: name.clone(),
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
            LspError::ServerTerminated { language } => Self::ServerTerminated {
                language: *language,
            },
            LspError::Timeout(msg) => Self::Timeout {
                message: msg.clone(),
            },
            LspError::RequestCancelled => Self::RequestCancelled,
            LspError::ServerError { code, message } => Self::ServerError {
                code: *code,
                message: message.clone(),
            },
            LspError::Protocol(msg) => Self::Protocol {
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
            WireLspError::ServerNotInstalled { name, install_hint } => {
                LspError::ServerNotInstalled { name, install_hint }
            }
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
            WireLspError::ServerTerminated { language } => LspError::ServerTerminated { language },
            WireLspError::Timeout { message } => LspError::Timeout(message),
            WireLspError::RequestCancelled => LspError::RequestCancelled,
            WireLspError::ServerError { code, message } => LspError::ServerError { code, message },
            WireLspError::Protocol { message } => LspError::Protocol(message),
            WireLspError::FileTooLarge {
                path,
                size_mb,
                limit_mb,
            } => LspError::FileTooLarge {
                path,
                size_mb,
                limit_mb,
            },
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
            install_hint: "rustup component add rust-analyzer".into(),
        });
        match recovered {
            LspError::ServerNotInstalled { name, install_hint } => {
                assert_eq!(name, "rust-analyzer");
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

    #[test]
    fn server_terminated_round_trips_language() {
        let recovered = round_trip(LspError::ServerTerminated {
            language: Language::Rust,
        });
        match recovered {
            LspError::ServerTerminated { language } => assert_eq!(language, Language::Rust),
            other => panic!("expected ServerTerminated, got {other:?}"),
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

    #[test]
    fn json_shape_uses_kind_discriminator() {
        let wire = WireLspError::from(&LspError::ServerNotInstalled {
            name: "rust-analyzer".into(),
            install_hint: "x".into(),
        });
        let json = serde_json::to_value(&wire).unwrap();
        assert_eq!(json["kind"], "server_not_installed");
        assert_eq!(json["name"], "rust-analyzer");
    }
}
