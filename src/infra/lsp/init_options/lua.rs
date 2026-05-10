use serde_json::{Value, json};
pub(super) fn lua_init_options() -> Value {
    json!({
        "runtime": {
            "version": "LuaJIT",
            "path": ["?.lua", "?/init.lua"]
        },
        "diagnostics": {
            "enable": true,
            "globals": ["vim", "describe", "it", "before_each", "after_each", "setup", "teardown"],
            "disable": [],
            "severity": {
                "undefined-global": "Error",
                "lowercase-global": "Warning"
            }
        },
        "workspace": {
            "checkThirdParty": false,
            "library": [],
            "ignoreDir": [".git", "node_modules", "build", "dist", ".luarocks", "lua_modules", ".cache"],
            "maxPreload": 1000,
            "preloadFileSize": 1048576
        },
        "completion": {
            "enable": true,
            "callSnippet": "Both"
        },
        "hint": {
            "enable": false,
            "paramType": false,
            "setType": false
        },
        "type": {
            "castNumberToInteger": true,
            "weakUnionCheck": true
        },
        "telemetry": {
            "enable": false
        }
    })
}
