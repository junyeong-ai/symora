use serde_json::{Value, json};
pub(super) fn scala_init_options() -> Value {
    json!({
        "bloopSbtAlreadyInstalled": false,
        "superMethodLensesEnabled": false,
        "showImplicitArguments": false,
        "showImplicitConversionsAndClasses": false,
        "showInferredType": false,
        "excludedPackages": [],
        "decorationProvider": false,
        "inlineDecorationProvider": false,
        "statusBarProvider": "off",
        "treeViewProvider": false,
        "debuggingProvider": true,
        "isHttpEnabled": true,
        "isExitOnShutdown": true,
        "globSyntax": "uri",
        "icons": "unicode",
        "inputBoxProvider": false,
        "isVirtualDocumentSupported": false,
        "openFilesOnRenameProvider": false,
        "quickPickProvider": false,
        "renameFileThreshold": 200,
        "testExplorerProvider": false,
        "openNewWindowProvider": false,
        "copyWorksheetOutputProvider": false,
        "doctorVisibilityProvider": false,
        "compilerOptions": {
            "completionCommand": null,
            "isCompletionItemDetailEnabled": true,
            "isCompletionItemDocumentationEnabled": true,
            "isCompletionItemResolve": true,
            "isHoverDocumentationEnabled": true,
            "isSignatureHelpDocumentationEnabled": true,
            "overrideDefFormat": "ascii",
            "snippetAutoIndent": false
        }
    })
}
