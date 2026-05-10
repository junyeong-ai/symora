use std::path::Path;

use serde_json::{Value, json};

use crate::models::lsp::path_to_uri;
pub(super) fn java_init_options(root_path: &Path) -> Value {
    let root_uri = path_to_uri(root_path);
    let java_home = detect_java_home();
    let gradle_home = std::env::var("GRADLE_HOME").ok();
    let gradle_user_home = std::env::var("GRADLE_USER_HOME")
        .ok()
        .or_else(|| dirs::home_dir().map(|h| h.join(".gradle").to_string_lossy().to_string()));
    let maven_settings = detect_maven_settings();
    let maven_user_settings = dirs::home_dir()
        .map(|h| h.join(".m2").join("settings.xml"))
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().to_string());

    json!({
        "bundles": [],
        "workspaceFolders": [root_uri],
        "settings": {
            "java": {
                "home": java_home,
                "jdt": {
                    "ls": {
                        "lombokSupport": { "enabled": true },
                        "protobufSupport": { "enabled": true },
                        "androidSupport": { "enabled": true },
                        "vmargs": "-XX:+UseParallelGC -XX:GCTimeRatio=4 -XX:AdaptiveSizePolicyWeight=90 -Xmx2G -Xms100m"
                    }
                },
                "configuration": {
                    "updateBuildConfiguration": "automatic",
                    "workspaceCacheLimit": 90,
                    "runtimes": []
                },
                "import": {
                    "gradle": {
                        "enabled": true,
                        "wrapper": { "enabled": true },
                        "offline": { "enabled": false },
                        "annotationProcessing": { "enabled": true },
                        "arguments": null,
                        "home": gradle_home,
                        "java": { "home": null },
                        "jvmArguments": null,
                        "user": { "home": gradle_user_home },
                        "version": null
                    },
                    "maven": {
                        "enabled": true,
                        "downloadSources": true,
                        "updateSnapshots": false,
                        "notCoveredPluginExecutionSeverity": "warning",
                        "defaultMojoExecutionAction": "ignore",
                        "disableTestClasspathFlag": false,
                        "globalSettings": maven_settings,
                        "userSettings": maven_user_settings
                    },
                    "exclusions": [
                        "**/node_modules/**",
                        "**/.metadata/**",
                        "**/archetype-resources/**",
                        "**/META-INF/maven/**",
                        "**/build/**",
                        "**/target/**",
                        "**/bin/**",
                        "**/out/**",
                        "**/generated/**",
                        "**/generated-sources/**",
                        "**/generated-test-sources/**",
                        "**/*Proto.java",
                        "**/*Grpc.java"
                    ],
                    "generatesMetadataFilesAtProjectRoot": false
                },
                "format": {
                    "enabled": true,
                    "insertSpaces": true,
                    "tabSize": 4,
                    "onType": { "enabled": true }
                },
                "compile": {
                    "nullAnalysis": {
                        "nonnull": [
                            "javax.annotation.Nonnull",
                            "org.eclipse.jdt.annotation.NonNull",
                            "org.springframework.lang.NonNull",
                            "lombok.NonNull",
                            "org.jetbrains.annotations.NotNull"
                        ],
                        "nullable": [
                            "javax.annotation.Nullable",
                            "org.eclipse.jdt.annotation.Nullable",
                            "org.springframework.lang.Nullable",
                            "org.jetbrains.annotations.Nullable"
                        ],
                        "mode": "automatic"
                    }
                },
                "inlayHints": {
                    "parameterNames": { "enabled": "literals" }
                },
                "references": {
                    "includeAccessors": true,
                    "includeDecompiledSources": true
                },
                "signatureHelp": { "enabled": true },
                "selectionRange": { "enabled": true },
                "completion": {
                    "enabled": true,
                    "favoriteStaticMembers": [
                        "org.junit.Assert.*",
                        "org.junit.jupiter.api.Assertions.*",
                        "org.mockito.Mockito.*",
                        "org.mockito.ArgumentMatchers.*",
                        "org.assertj.core.api.Assertions.*"
                    ],
                    "filteredTypes": [
                        "com.sun.*",
                        "io.micrometer.shaded.*",
                        "java.awt.*",
                        "jdk.*",
                        "sun.*"
                    ],
                    "guessMethodArguments": true,
                    "importOrder": ["java", "javax", "org", "com", ""]
                },
                "sources": {
                    "organizeImports": {
                        "starThreshold": 99,
                        "staticStarThreshold": 99
                    }
                },
                "cleanup": {
                    "actionsOnSave": []
                }
            }
        }
    })
}

fn detect_java_home() -> Option<String> {
    if let Ok(java_home) = std::env::var("JAVA_HOME") {
        return Some(java_home);
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("/usr/libexec/java_home")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

fn detect_maven_settings() -> Option<String> {
    std::env::var("M2_HOME")
        .ok()
        .map(|h| {
            std::path::PathBuf::from(h)
                .join("conf")
                .join("settings.xml")
        })
        .or_else(|| {
            std::env::var("MAVEN_HOME").ok().map(|h| {
                std::path::PathBuf::from(h)
                    .join("conf")
                    .join("settings.xml")
            })
        })
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().to_string())
}
