//! Source mutations share project-boundary validation, revision-checked
//! writes, unified previews, and typed output (`EditOutput`).
//!
//! Symbol-targeted subcommands (`replace-body`, `insert-before`,
//! `insert-after`) operate on whole lines of the symbol's full
//! declaration span — the body the agent supplies is taken verbatim,
//! indentation included. `replace` and `pattern` are character-precise;
//! all columns at this boundary are 1-indexed *character* counts (the
//! AST layer's byte columns are converted before they reach the core).

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::cli::response::{LineRange, Section};
use crate::cli::utils::shell_arg;
use crate::models::lsp::IndexingDegradation;
use crate::models::symbol::{Language, Symbol};
use crate::utils::char_to_byte_index;

mod pattern;
mod splice;
mod target;
mod workspace_edit;
mod write;

use pattern::pattern_edit;
use splice::{FileDocument, LineSplice, char_splice, invalid_range, to_lines};
use target::{Addressed, resolve_symbol, symbol_line_span};

pub use workspace_edit::{AppliedFileChange, apply_workspace_edits};
pub(crate) use write::{atomic_write, lock_edits, refresh_store_files};

#[derive(Args, Debug)]
pub struct EditArgs {
    #[command(subcommand)]
    pub command: EditCommand,
}

#[derive(Subcommand, Debug)]
pub enum EditCommand {
    /// Replace a symbol's entire definition — the whole lines `symbols
    /// --body` returns, from the decorator, attribute or doc-comment lines it
    /// opens with through its closing brace — pass the complete definition,
    /// not just the inner code. For a raw character range use `replace`.
    ReplaceBody {
        /// Target: `file:line[:col]` (location) or file path (with --symbol)
        target: String,

        /// Symbol path/pattern when target is a file (name, "Class/method", or "*/method")
        #[arg(short = 's', long)]
        symbol: Option<String>,

        /// Complete replacement for the symbol's definition — signature,
        /// braces, and body, indentation included. Pass `-` to read from
        /// stdin.
        #[arg(long)]
        body: String,

        /// Preview the change without writing to disk.
        #[arg(long)]
        dry_run: bool,

        /// After an applied edit, pull LSP diagnostics for the file and
        /// attach them to the output. Ignored on dry runs.
        #[arg(long)]
        with_diagnostics: bool,

        /// After an applied edit, also pull diagnostics for the edited
        /// symbol's caller files (its one-hop references), closing the
        /// read->edit->verify loop across callers. Ignored on dry runs.
        #[arg(long)]
        verify_callers: bool,
    },

    /// Insert source lines immediately before a symbol.
    InsertBefore {
        /// Target: `file:line[:col]` (location) or file path (with --symbol)
        target: String,

        /// Symbol path/pattern when target is a file (name, "Class/method", or "*/method")
        #[arg(short = 's', long)]
        symbol: Option<String>,

        /// Source code to insert. Pass `-` to read from stdin.
        #[arg(long)]
        code: String,

        /// Preview the change without writing to disk.
        #[arg(long)]
        dry_run: bool,

        /// After an applied edit, pull LSP diagnostics for the file and
        /// attach them to the output. Ignored on dry runs.
        #[arg(long)]
        with_diagnostics: bool,

        /// After an applied edit, also pull diagnostics for the edited
        /// symbol's caller files (its one-hop references), closing the
        /// read->edit->verify loop across callers. Ignored on dry runs.
        #[arg(long)]
        verify_callers: bool,
    },

    /// Insert source lines immediately after a symbol.
    InsertAfter {
        /// Target: `file:line[:col]` (location) or file path (with --symbol)
        target: String,

        /// Symbol path/pattern when target is a file (name, "Class/method", or "*/method")
        #[arg(short = 's', long)]
        symbol: Option<String>,

        /// Source code to insert. Pass `-` to read from stdin.
        #[arg(long)]
        code: String,

        /// Preview the change without writing to disk.
        #[arg(long)]
        dry_run: bool,

        /// After an applied edit, pull LSP diagnostics for the file and
        /// attach them to the output. Ignored on dry runs.
        #[arg(long)]
        with_diagnostics: bool,

        /// After an applied edit, also pull diagnostics for the edited
        /// symbol's caller files (its one-hop references), closing the
        /// read->edit->verify loop across callers. Ignored on dry runs.
        #[arg(long)]
        verify_callers: bool,
    },

    /// Delete a symbol's full definition. Always reports references
    /// outside the deleted span that would dangle; with
    /// `--expect-no-references` the report becomes a checked
    /// precondition and the delete is refused while they exist or
    /// cannot be verified.
    Delete {
        /// Target: `file:line[:col]` (location) or file path (with --symbol)
        target: String,

        /// Symbol path/pattern when target is a file (name, "Class/method", or "*/method")
        #[arg(short = 's', long)]
        symbol: Option<String>,

        /// Make verified reference-freedom a precondition: refuse the delete
        /// (no write) unless the reference check verified zero references
        /// outside the deleted span. Fail-closed: an unsupported or failed
        /// check, or a zero computed under degraded indexing, also refuses.
        /// Evaluated on dry runs too.
        #[arg(long)]
        expect_no_references: bool,

        /// Preview the deletion without writing to disk.
        #[arg(long)]
        dry_run: bool,

        /// After an applied edit, pull LSP diagnostics for the file and
        /// attach them to the output. Ignored on dry runs.
        #[arg(long)]
        with_diagnostics: bool,
    },

    /// Replace a raw character range (no symbol resolution). For whole
    /// symbols use `replace-body`.
    Replace {
        /// Start location (file:line[:column]; an omitted column means column 1)
        start: String,

        /// End location (file:line[:column]), exclusive. Defaults to the
        /// end of the start line.
        #[arg(short, long)]
        end: Option<String>,

        /// New text. Pass `-` to read from stdin.
        #[arg(short, long)]
        text: String,

        /// Assert the live text spanned by the range equals this before
        /// replacing. The edit is refused (no write) when it differs, so a
        /// range read against a now-stale revision can't be clobbered.
        /// `\r\n` and `\n` compare equal; every other character must match
        /// exactly.
        #[arg(long)]
        expect: Option<String>,

        /// Preview the change without writing to disk.
        #[arg(long)]
        dry_run: bool,

        /// After an applied edit, pull LSP diagnostics for the file and
        /// attach them to the output. Ignored on dry runs.
        #[arg(long)]
        with_diagnostics: bool,
    },

    /// Replace code matching a tree-sitter AST pattern. For post-edit
    /// health checks run `symora diagnostics <file>` — pattern output is
    /// per-match, so file-level diagnostics are not attached inline.
    Pattern {
        /// File path to edit
        file: String,

        /// Tree-sitter pattern (e.g., "(function_item name: (identifier) @name)")
        #[arg(short, long)]
        pattern: String,

        /// Language for tree-sitter grammar
        #[arg(short, long)]
        lang: String,

        /// New text to replace matched code. Pass `-` to read from stdin.
        #[arg(short, long)]
        text: String,

        /// Match index to replace (0-indexed); use "all" to replace all matches
        #[arg(short, long, default_value = "0")]
        index: String,

        /// Preview the matches without writing to disk.
        #[arg(long)]
        dry_run: bool,
    },
}

pub async fn execute(args: EditArgs, app: &App) -> Result<()> {
    if let Err(e) = run(args.command, app).await {
        app.output.print_error(e);
    }
    Ok(())
}

async fn run(command: EditCommand, app: &App) -> Result<()> {
    ensure_source(&command)?;
    let dry_run = match &command {
        EditCommand::ReplaceBody { dry_run, .. }
        | EditCommand::InsertBefore { dry_run, .. }
        | EditCommand::InsertAfter { dry_run, .. }
        | EditCommand::Delete { dry_run, .. }
        | EditCommand::Replace { dry_run, .. }
        | EditCommand::Pattern { dry_run, .. } => *dry_run,
    };
    let _lock = (!dry_run).then(|| lock_edits(app.root())).transpose()?;
    match command {
        EditCommand::ReplaceBody {
            target,
            symbol,
            body,
            dry_run,
            with_diagnostics,
            verify_callers,
        } => {
            let addressed = resolve_symbol(app, &target, symbol).await?;
            symbol_edit(
                app,
                "replace_body",
                &addressed,
                dry_run,
                with_diagnostics,
                verify_callers,
                |span| LineSplice {
                    at: span.start as usize - 1,
                    removed: (span.end - span.start + 1) as usize,
                    new_lines: to_lines(&body),
                },
            )
            .await
        }
        EditCommand::InsertBefore {
            target,
            symbol,
            code,
            dry_run,
            with_diagnostics,
            verify_callers,
        } => {
            let addressed = resolve_symbol(app, &target, symbol).await?;
            symbol_edit(
                app,
                "insert_before",
                &addressed,
                dry_run,
                with_diagnostics,
                verify_callers,
                |span| LineSplice {
                    at: span.start as usize - 1,
                    removed: 0,
                    new_lines: to_lines(&code),
                },
            )
            .await
        }
        EditCommand::InsertAfter {
            target,
            symbol,
            code,
            dry_run,
            with_diagnostics,
            verify_callers,
        } => {
            let addressed = resolve_symbol(app, &target, symbol).await?;
            symbol_edit(
                app,
                "insert_after",
                &addressed,
                dry_run,
                with_diagnostics,
                verify_callers,
                |span| LineSplice {
                    at: span.end as usize,
                    removed: 0,
                    new_lines: to_lines(&code),
                },
            )
            .await
        }
        EditCommand::Delete {
            target,
            symbol,
            expect_no_references,
            dry_run,
            with_diagnostics,
        } => {
            let addressed = resolve_symbol(app, &target, symbol).await?;
            delete_symbol(
                app,
                &addressed,
                dry_run,
                with_diagnostics,
                expect_no_references,
            )
            .await
        }
        EditCommand::Replace {
            start,
            end,
            text,
            expect,
            dry_run,
            with_diagnostics,
        } => {
            let start_loc =
                ParsedLocation::parse(&start)?.to_absolute_with_root(Some(app.root()))?;
            let (end_line, end_col) = match end {
                Some(e) => {
                    let loc = ParsedLocation::parse(&e)?.to_absolute_with_root(Some(app.root()))?;
                    if loc.file != start_loc.file {
                        anyhow::bail!(invalid_range(format!(
                            "End location is in a different file ({}) than start ({})",
                            loc.file.display(),
                            start_loc.file.display(),
                        )));
                    }
                    (loc.line, Some(loc.column))
                }
                None => (start_loc.line, None),
            };

            let doc = FileDocument::load(&start_loc.file, dry_run)?;
            let splice = char_splice(
                &doc.lines,
                start_loc.line,
                start_loc.column,
                end_line,
                end_col,
                &text,
                expect.as_deref(),
            )?;
            let span = LineRange {
                start: start_loc.line,
                end: end_line,
            };
            let mut output = doc.commit(app, "replace", splice, span, None, dry_run)?;
            output.diagnostics =
                pull_diagnostics(app, &start_loc.file, dry_run, with_diagnostics).await;
            app.output.print_success(output);
            finish(app, &start_loc.file, dry_run).await;
            Ok(())
        }
        EditCommand::Pattern {
            file,
            pattern,
            lang,
            text,
            index,
            dry_run,
        } => pattern_edit(app, &file, &pattern, &lang, &text, &index, dry_run).await,
    }
}

/// Refuse source that would not do what the edit names. A definition
/// replaced with blank lines is a delete, which `edit delete` makes while
/// reporting the references it leaves dangling, and inserting an empty
/// string changes nothing.
fn ensure_source(command: &EditCommand) -> Result<()> {
    match command {
        EditCommand::ReplaceBody { body, .. } if body.trim().is_empty() => {
            anyhow::bail!(
                crate::cli::OutputError::invalid("The replacement definition is blank").with_hint(
                    "To remove the declaration, run `symora edit delete`, which reports the \
                     references its removal leaves dangling",
                )
            )
        }
        EditCommand::InsertBefore { code, .. } | EditCommand::InsertAfter { code, .. }
            if code.is_empty() =>
        {
            anyhow::bail!(crate::cli::OutputError::invalid(
                "The code to insert is empty"
            ))
        }
        _ => Ok(()),
    }
}

/// Shared tail for symbol-targeted line edits: resolve the span with the
/// stale-range guard, splice, emit one `EditOutput`, refresh the index.
async fn symbol_edit(
    app: &App,
    operation: &'static str,
    addressed: &Addressed,
    dry_run: bool,
    with_diagnostics: bool,
    verify_callers: bool,
    make_splice: impl FnOnce(&LineRange) -> LineSplice,
) -> Result<()> {
    let (file, symbol) = (addressed.file.as_path(), &addressed.symbol);
    let doc = FileDocument::load(file, dry_run)?;
    if crate::infra::hash_content(&doc.content) != addressed.expected_hash {
        anyhow::bail!(stale_revision(format!(
            "{} changed while resolving the symbol",
            file.display()
        )));
    }
    let span = symbol_line_span(symbol, doc.lines.len())?;
    let splice = make_splice(&span);
    if splice.removed > 0 {
        ensure_anchor_not_stale(symbol, &doc.lines)?;
        ensure_exclusive_line_ownership(&doc.lines, symbol, &span, &addressed.declared_with)?;
    }
    // Resolve the caller files BEFORE the edit lands: editing a symbol does not
    // move which files reference it, and resolving against the pre-edit state
    // avoids any post-edit position drift. The diagnostics for those files are
    // pulled AFTER the write, so they reflect the edit's effect.
    let caller_files = if verify_callers && !dry_run {
        Some(collect_caller_files(app, addressed).await)
    } else {
        None
    };
    let target = Some((symbol.path().to_string(), symbol.kind.to_string()));
    let mut output = doc.commit(app, operation, splice, span, target, dry_run)?;
    // verify-callers must cover SAME-FILE callers too — they live in the edited
    // file, which collect_caller_files deliberately excludes (to avoid
    // double-diagnosis). So verify-callers implies the edited file's own
    // diagnostics even without --with-diagnostics; otherwise a same-file
    // call-site break would be silently dropped and the empty caller list would
    // read as authoritative.
    output.diagnostics =
        pull_diagnostics(app, file, dry_run, with_diagnostics || verify_callers).await;
    if let Some(callers) = caller_files {
        // Caller diagnostics must judge the edit's NEW signature. Sync the
        // edited file to the language server before pulling them. `finish`
        // re-syncs after this returns, but that later note is a no-op (the
        // content is already current), so there is no real double work. Without
        // this pre-sync the caller files are analysed against the server's stale
        // pre-edit view of the edited file (its overlay was opened at the old
        // content during the reference lookup), so a freshly introduced caller
        // break reads as clean — a false negative independent of --with-diagnostics.
        let edited = [file.to_path_buf()];
        app.lsp.note_files_edited(&edited).await;
        output.caller_verification = Some(verify_caller_files(app, callers).await);
    }
    app.output.print_success(output);
    finish(app, file, dry_run).await;
    Ok(())
}

/// Whole-line operations (replace-body, delete) must not take neighbour
/// code with them. This is an exact check, not a heuristic: if the
/// symbol's declaration declares other names too (`declared_with`: `var a,
/// b = f(…)`), its first line has non-whitespace before its declaration
/// start, or its last line has non-whitespace after its declared end
/// column, the operation is refused with a character-precise alternative —
/// never a silent over-splice.
fn ensure_exclusive_line_ownership(
    lines: &[String],
    symbol: &Symbol,
    span: &LineRange,
    declared_with: &[String],
) -> Result<()> {
    let refused = |message: String| {
        anyhow::Error::new(
            crate::cli::OutputError::unsupported(message)
                .with_hint("Use `edit replace` for character-precise control"),
        )
    };
    let shared = |what: &str| {
        refused(format!(
            "Symbol '{}' shares its {what} line with other code; \
             whole-line edits would remove it",
            symbol.name,
        ))
    };
    if !declared_with.is_empty() {
        let names: Vec<String> = declared_with
            .iter()
            .map(|name| format!("'{name}'"))
            .collect();
        return Err(refused(format!(
            "Symbol '{}' is declared together with {}; whole-line edits would remove {} too",
            symbol.name,
            names.join(", "),
            if names.len() == 1 { "it" } else { "them" },
        )));
    }

    let (_, start_col) = symbol.location.effective_start();
    let first = &lines[span.start as usize - 1];
    let prefix = &first[..char_to_byte_index(first, (start_col.saturating_sub(1)) as usize)];
    if !prefix.trim().is_empty() {
        return Err(shared("first"));
    }

    // `end_column` is the exclusive end (1-indexed); without it the LSP
    // gave no end position and there is nothing to check against.
    if let Some(end_col) = symbol.location.end_column {
        let last = &lines[span.end as usize - 1];
        let suffix = &last[char_to_byte_index(last, (end_col.saturating_sub(1)) as usize)..];
        if !suffix.trim().is_empty() {
            return Err(shared("last"));
        }
    }
    Ok(())
}

/// Refuse a destructive splice whose resolved anchor no longer matches the
/// bytes on disk. The symbol's span is computed from LSP/index state that can
/// lag the file about to be spliced (a stale index row, a server overlay
/// behind disk); if the file changed since, the span may now cover a different
/// symbol entirely, and a delete/replace there is unrecoverable. A named
/// symbol absent from its resolved NAME line is the unambiguous signal of
/// that drift — fail closed and send the agent back to re-resolve, the same
/// discipline `--expect` enforces for character-range edits. Anonymous or
/// non-identifier names carry no such textual anchor, so they are left to the
/// EOF and line-ownership guards rather than risk a false refusal.
fn ensure_anchor_not_stale(symbol: &Symbol, lines: &[String]) -> Result<()> {
    let name = symbol.name.trim();
    let is_identifier = !name.is_empty() && name.chars().all(is_identifier_char);
    if !is_identifier {
        return Ok(());
    }
    // Check the NAME line — where the identifier literally appears — NOT the
    // declaration-range start: for a doc-commented or attributed item the range
    // begins on a leading `///`/`#[…]` line the name never occurs on, and
    // anchoring there would falsely refuse a destructive edit on every
    // documented symbol. An absent name line means the file drifted under the
    // resolved position — fail closed either way.
    let name_line = symbol.location.line;
    let on_name_line = name_line
        .checked_sub(1)
        .and_then(|i| lines.get(i as usize))
        .is_some_and(|declaration| line_has_whole_identifier(declaration, name));
    if !on_name_line {
        anyhow::bail!(stale_revision(format!(
            "Symbol '{name}' is no longer on line {name_line} of the current file — it moved or was \
             replaced since it was resolved, so a destructive edit there would hit the wrong \
             code. Re-query its position (search symbols / symbols) and rerun the edit."
        )));
    }
    Ok(())
}

fn is_identifier_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Whether `name` occurs in `line` as a WHOLE identifier — bounded by a
/// non-identifier character or a line edge on both sides — not merely as a
/// substring. A substring match would let a stale `process` pass on a
/// replacement line like `fn preprocess()` or `let process_status = …`,
/// approving a destructive edit on the wrong code (the failure this guard
/// exists to prevent).
fn line_has_whole_identifier(line: &str, name: &str) -> bool {
    line.match_indices(name).any(|(idx, matched)| {
        let before = line[..idx].chars().next_back();
        let after = line[idx + matched.len()..].chars().next();
        before.is_none_or(|c| !is_identifier_char(c))
            && after.is_none_or(|c| !is_identifier_char(c))
    })
}

/// Delete = splice to zero lines, plus the safety check that always
/// runs — dry-run included. The destructive path never skips it. With
/// `expect_no_references`, the check's verified-zero result becomes a
/// precondition for the write.
async fn delete_symbol(
    app: &App,
    addressed: &Addressed,
    dry_run: bool,
    with_diagnostics: bool,
    expect_no_references: bool,
) -> Result<()> {
    let (file, symbol) = (addressed.file.as_path(), &addressed.symbol);
    let doc = FileDocument::load(file, dry_run)?;
    if crate::infra::hash_content(&doc.content) != addressed.expected_hash {
        anyhow::bail!(stale_revision(format!(
            "{} changed while resolving the symbol",
            file.display()
        )));
    }
    let span = symbol_line_span(symbol, doc.lines.len())?;
    ensure_anchor_not_stale(symbol, &doc.lines)?;
    ensure_exclusive_line_ownership(&doc.lines, symbol, &span, &addressed.declared_with)?;

    let check = check_dangling_references(app, addressed, &span).await;
    if expect_no_references {
        ensure_no_dangling_references(&symbol.name, &check)?;
    }

    let splice = LineSplice {
        at: span.start as usize - 1,
        removed: (span.end - span.start + 1) as usize,
        new_lines: Vec::new(),
    };
    let target = Some((symbol.path().to_string(), symbol.kind.to_string()));
    let mut output = doc.commit(app, "delete", splice, span, target, dry_run)?;
    match check {
        ReferenceCheck::Checked { section, .. } => output.dangling_references = Some(section),
        ReferenceCheck::Unverifiable(status) => output.references_status = Some(status),
        ReferenceCheck::Unplaced => output.references_status = Some("unavailable"),
    }
    output.diagnostics = pull_diagnostics(app, file, dry_run, with_diagnostics).await;
    app.output.print_success(output);
    finish(app, file, dry_run).await;
    Ok(())
}

/// Cap on inline dangling-reference listings; the full set stays one
/// `find_references` call away and the section says it was truncated.
const DELETE_REFS_DISPLAY_LIMIT: usize = 50;

/// Outcome of the pre-delete reference lookup. A check that ran fills
/// `EditOutput`'s `dangling_references`, every other its exactly-one-of
/// `references_status`, so the presence rule is enforced at the producer's
/// type rather than by convention.
enum ReferenceCheck {
    /// The lookup ran; the section lists span-filtered dangling
    /// references and carries the `indexing` marker when the count is a
    /// lower bound, and `refs_command` lists the same references.
    Checked {
        section: Section<crate::cli::response::LocationOutput>,
        refs_command: String,
    },
    /// The lookup could not run; the status string is the
    /// `references_status` value emitted on `EditOutput`
    /// (`"unsupported"` | `"unavailable"`).
    Unverifiable(&'static str),
    /// The language server reads no position as the declaration — the
    /// grammar answered for it, and no definition the server names at its
    /// position is it — so it was not asked. Emitted as
    /// `references_status: unavailable`.
    Unplaced,
}

/// References at the *name* position (not the declaration start the
/// splice uses — nothing references the `pub` keyword) of the symbol the
/// language server reads as the deleted declaration
/// ([`Addressed::served`]), that live outside the deleted span. Honest per
/// invariant 4: when the language can't answer, return a status instead of
/// implying "none".
async fn check_dangling_references(
    app: &App,
    addressed: &Addressed,
    span: &LineRange,
) -> ReferenceCheck {
    use crate::cli::response::LocationOutput;

    let file = addressed.file.as_path();
    use crate::infra::lsp::capabilities::{LspFeature, SupportLevel, get_support_level};

    if get_support_level(Language::from_path(file), LspFeature::FindReferences)
        == SupportLevel::None
    {
        return ReferenceCheck::Unverifiable("unsupported");
    }
    let symbol = match addressed.served(app).await {
        Ok(Some(symbol)) => symbol,
        Ok(None) => return ReferenceCheck::Unplaced,
        Err(e) => {
            tracing::warn!("Definition lookup for delete's reference check failed: {e}");
            return ReferenceCheck::Unverifiable("unavailable");
        }
    };

    let refs = match app
        .lsp
        .find_references(file, symbol.location.line, symbol.location.column)
        .await
    {
        Ok(refs) => refs,
        Err(e) => {
            tracing::warn!("Reference check for delete failed: {e}");
            return ReferenceCheck::Unverifiable("unavailable");
        }
    };

    let dangling: Vec<LocationOutput> = refs
        .data
        .iter()
        .filter(|r| r.file != file || r.line < span.start || r.line > span.end)
        .map(|r| LocationOutput::from_location(r, app.output.root()))
        .collect();
    let total = dangling.len();
    // A server still indexing returns a *lower bound*, not the truth —
    // the canonical `indexing` marker (captured when the reference query
    // ran, not re-read afterwards) keeps a cold-start zero from reading
    // as "confirmed no references".
    let indexing = refs.indexing;
    let mut section = Section::with_total(
        dangling
            .into_iter()
            .take(DELETE_REFS_DISPLAY_LIMIT)
            .collect(),
        total,
    )
    .with_indexing(indexing);
    if total == 0 {
        section = section.with_hints(Vec::from_iter(
            crate::cli::analysis::member_reach_hint_of(
                app.lsp.as_ref(),
                file,
                symbol,
                app.output.root(),
            )
            .await,
        ));
    }
    let refs_command = format!(
        "symora refs -- {}",
        shell_arg(&format!(
            "{}:{}:{}",
            app.output.relative_path(file),
            symbol.location.line,
            symbol.location.column
        )),
    );
    if total > DELETE_REFS_DISPLAY_LIMIT {
        section = section.with_next_commands(vec![refs_command.clone()]);
    }
    ReferenceCheck::Checked {
        section,
        refs_command,
    }
}

/// The no-references precondition, fail-closed: the ONLY state that
/// passes is a completed check with zero dangling references and no
/// indexing degradation. A non-answer or a documented lower bound must
/// never read as "confirmed reference-free" (invariant 4) — every other
/// state refuses, naming its exact reason in the message and a working
/// alternative in the hint.
fn ensure_no_dangling_references(symbol_name: &str, check: &ReferenceCheck) -> Result<()> {
    fn refuse(message: String, hint: String) -> anyhow::Error {
        anyhow::Error::new(crate::cli::OutputError::precondition_failed(message).with_hint(hint))
    }

    match check {
        ReferenceCheck::Unverifiable(status) => {
            let message = format!(
                "Delete of '{symbol_name}' refused: the no-references precondition \
                 cannot be verified (references_status: {status})"
            );
            let hint = if *status == "unsupported" {
                format!(
                    "Reference lookup is unsupported for this language; verify call \
                     sites manually (try: symora search content -- {}) and \
                     rerun without --expect-no-references.",
                    shell_arg(symbol_name)
                )
            } else {
                "The reference lookup failed; retry once, check 'symora doctor', or \
                 verify manually and rerun without --expect-no-references."
                    .to_string()
            };
            anyhow::bail!(refuse(message, hint))
        }
        ReferenceCheck::Unplaced => anyhow::bail!(refuse(
            format!(
                "Delete of '{symbol_name}' refused: the no-references precondition \
                 cannot be verified (references_status: unavailable)"
            ),
            format!(
                "The language server's symbols lack this declaration, and no definition the \
                 server names at its position is it, so its references cannot be asked for; \
                 verify call sites manually (try: symora search content -- {}) and rerun \
                 without --expect-no-references.",
                shell_arg(symbol_name)
            ),
        )),
        ReferenceCheck::Checked {
            section,
            refs_command,
        } if section.count > 0 => {
            let count = section.count;
            let message = match section.indexing {
                None => format!(
                    "Delete of '{symbol_name}' refused: {count} dangling references \
                     outside the deleted span violate the no-references precondition"
                ),
                Some(degradation) => format!(
                    "Delete of '{symbol_name}' refused: at least {count} dangling \
                     references outside the deleted span violate the no-references \
                     precondition (indexing: {} — the count is a lower bound)",
                    degradation.as_str(),
                ),
            };
            anyhow::bail!(refuse(
                message,
                format!(
                    "List them: {refs_command}. Fix those call sites and retry, or \
                     rerun without --expect-no-references."
                ),
            ))
        }
        ReferenceCheck::Checked {
            section,
            refs_command,
        } => match section.indexing {
            Some(degradation) => anyhow::bail!(refuse(
                format!(
                    "Delete of '{symbol_name}' refused: the reference count of 0 is a \
                     lower bound under degraded indexing (indexing: {}), which does \
                     not verify the no-references precondition",
                    degradation.as_str(),
                ),
                match degradation {
                    IndexingDegradation::TimedOut => format!(
                        "The readiness wait exhausted its budget; retrying may help once the \
                         language server confirms readiness. Otherwise check `{refs_command}` \
                         yourself and rerun without --expect-no-references."
                    ),
                    IndexingDegradation::Unconfirmed => format!(
                        "The language server has not confirmed workspace readiness, and \
                         retrying alone does not establish completeness. Check \
                         `{refs_command}` yourself and rerun without --expect-no-references."
                    ),
                },
            )),
            None => Ok(()),
        },
    }
}

/// Post-edit diagnostics pull, gated on the flag and on the edit having
/// actually hit disk. The tri-state from the service layer is passed
/// through verbatim — an empty list under `unconfirmed` stays labelled
/// as unknown, never implied clean.
async fn pull_diagnostics(
    app: &App,
    file: &Path,
    dry_run: bool,
    with_diagnostics: bool,
) -> Option<crate::cli::response::EditDiagnostics> {
    if !with_diagnostics || dry_run {
        return None;
    }
    Some(diagnostics_for(app, file).await)
}

/// Pull one file's LSP diagnostics into the honest tri-state `EditDiagnostics`.
/// The single place that maps a diagnostics report (or a failed pull) to the
/// `ok`/`unconfirmed`/`unsupported`/`unavailable` status, shared by the
/// edited-file and caller-file verification paths.
async fn diagnostics_for(app: &App, file: &Path) -> crate::cli::response::EditDiagnostics {
    use crate::cli::response::{DiagnosticOutput, EditDiagnostics};
    use crate::models::diagnostic::DiagnosticsStatus;

    match app.lsp.diagnostics(file).await {
        Ok(report) => EditDiagnostics {
            status: match report.status {
                DiagnosticsStatus::Ok => "ok",
                DiagnosticsStatus::Unconfirmed => "unconfirmed",
                DiagnosticsStatus::Unsupported => "unsupported",
            },
            count: Some(report.items.len()),
            items: report.items.iter().map(DiagnosticOutput::from).collect(),
        },
        Err(e) => {
            tracing::warn!("Diagnostics pull failed for {}: {e}", file.display());
            EditDiagnostics {
                status: "unavailable",
                count: None,
                items: Vec::new(),
            }
        }
    }
}

/// The most caller files `--verify-callers` will pull diagnostics for. Bounds
/// the LSP fan-out; the overflow is disclosed via the `Section`'s truncation.
const VERIFY_CALLERS_FILE_LIMIT: usize = 10;

/// The one-hop caller files of `symbol`, resolved before the edit lands.
struct CallerFiles {
    files: Vec<PathBuf>,
    total: usize,
    indexing: Option<IndexingDegradation>,
    status: Option<&'static str>,
}

/// Resolve the distinct files that reference the addressed declaration,
/// EXCLUDING the edited file — its same-file callers are covered by the edit's
/// own edited-file diagnostics (which verify-callers forces on), so listing it
/// here too would double-diagnose it. Capped and deterministic. `status` is set
/// only when the reference lookup could not run — never paired with files —
/// which includes a declaration the language server reads at no position
/// ([`Addressed::served`]).
async fn collect_caller_files(app: &App, addressed: &Addressed) -> CallerFiles {
    use crate::infra::lsp::capabilities::{LspFeature, SupportLevel, get_support_level};

    let file = addressed.file.as_path();
    let empty = |status| CallerFiles {
        files: Vec::new(),
        total: 0,
        indexing: None,
        status: Some(status),
    };
    if get_support_level(Language::from_path(file), LspFeature::FindReferences)
        == SupportLevel::None
    {
        return empty("unsupported");
    }
    let symbol = match addressed.served(app).await {
        Ok(Some(symbol)) => symbol,
        Ok(None) => return empty("unavailable"),
        Err(e) => {
            tracing::warn!("verify-callers definition lookup failed: {e}");
            return empty("unavailable");
        }
    };
    let refs = match app
        .lsp
        .find_references(file, symbol.location.line, symbol.location.column)
        .await
    {
        Ok(refs) => refs,
        Err(e) => {
            tracing::warn!("verify-callers reference lookup failed: {e}");
            return empty("unavailable");
        }
    };
    let indexing = refs.indexing;
    // Compare candidates by canonical form: the edited file's path is canonical,
    // but a server-returned reference URI may be spelled differently (symlinked
    // root, case-folded FS), and an exact-string compare would then list the
    // edited file as its own caller. Dedup on the same canonical key.
    let canonical_edited = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    let mut seen = std::collections::HashSet::new();
    let mut files: Vec<PathBuf> = refs
        .data
        .iter()
        .map(|r| r.file.clone())
        .filter(|f| {
            let canon = f.canonicalize().unwrap_or_else(|_| f.clone());
            canon != canonical_edited && seen.insert(canon)
        })
        .collect();
    files.sort();
    let total = files.len();
    files.truncate(VERIFY_CALLERS_FILE_LIMIT);
    CallerFiles {
        files,
        total,
        indexing,
        status: None,
    }
}

/// Pull each caller file's diagnostics concurrently and assemble the honest
/// `CallerVerification`. Wall-clock is bounded by one diagnostics-wait budget
/// (the pulls run in parallel) and the file cap.
async fn verify_caller_files(
    app: &App,
    callers: CallerFiles,
) -> crate::cli::response::CallerVerification {
    use crate::cli::response::{CallerFileDiagnostics, CallerVerification};

    let items = futures::future::join_all(callers.files.iter().map(|f| async move {
        CallerFileDiagnostics {
            file: app.output.relative_path(f),
            diagnostics: diagnostics_for(app, f).await,
        }
    }))
    .await;

    CallerVerification {
        callers: Section::with_total(items, callers.total).with_indexing(callers.indexing),
        status: callers.status,
    }
}

async fn finish(app: &App, file: &Path, dry_run: bool) {
    if !dry_run {
        refresh_store_files(app, std::slice::from_ref(&file.to_path_buf())).await;
    }
}

/// A computed or asserted edit range no longer matches the on-disk file —
/// the analysis (LSP range, AST match, or an `--expect` assertion) ran
/// against a different revision than the bytes on disk. Surfaced as
/// `ErrorCode::Conflict` so an agent branches on it to re-read and retry
/// instead of treating a recoverable staleness as an internal failure.
fn stale_revision(message: String) -> anyhow::Error {
    anyhow::Error::new(crate::cli::OutputError::conflict(message))
}

#[cfg(test)]
mod test_support {
    use std::path::PathBuf;

    use crate::models::symbol::{Location, Symbol, SymbolKind};

    pub(super) fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    pub(super) fn sample_symbol(start: u32, end: u32) -> Symbol {
        Symbol::new(
            "process".to_string(),
            SymbolKind::Function,
            Location::full(PathBuf::from("/tmp/foo.rs"), start, 1, start, 1, end, 1),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{lines, sample_symbol};
    use super::*;
    use crate::models::symbol::{Location, SymbolKind};

    /// The only state that satisfies the no-references precondition: the
    /// check ran, found zero dangling references, with no degradation.
    #[test]
    fn no_references_guard_passes_on_verified_zero() {
        let check = ReferenceCheck::Checked {
            section: Section::new(vec![]),
            refs_command: "symora refs src/foo.rs:2:1".into(),
        };
        assert!(ensure_no_dangling_references("process", &check).is_ok());
    }

    /// Dangling references refuse with the pre-cap total in the message
    /// and the ready-to-run refs command in the hint.
    #[test]
    fn no_references_guard_refuses_dangling_references() {
        use crate::cli::{ErrorCode, OutputError};
        let check = ReferenceCheck::Checked {
            section: Section::with_total(Vec::new(), 2),
            refs_command: "symora refs src/foo.rs:2:1".into(),
        };
        let err = ensure_no_dangling_references("process", &check).unwrap_err();
        let out: OutputError = err.into();
        assert!(matches!(out.code, ErrorCode::PreconditionFailed));
        assert!(out.message.contains("2 dangling references"));
        assert!(out.message.contains("outside the deleted span"));
        assert!(out.hint.unwrap().contains("symora refs src/foo.rs:2:1"));
    }

    /// An unsupported reference lookup fails closed, disclosing the
    /// status verbatim and routing to the manual alternative.
    #[test]
    fn no_references_guard_refuses_unsupported_status() {
        use crate::cli::{ErrorCode, OutputError};
        let check = ReferenceCheck::Unverifiable("unsupported");
        let err = ensure_no_dangling_references("process", &check).unwrap_err();
        let out: OutputError = err.into();
        assert!(matches!(out.code, ErrorCode::PreconditionFailed));
        assert!(out.message.contains("references_status: unsupported"));
        assert!(out.hint.unwrap().contains("search content"));
    }

    /// A failed lookup also fails closed — "could not check" never reads
    /// as "no references".
    #[test]
    fn no_references_guard_refuses_unavailable_status() {
        use crate::cli::{ErrorCode, OutputError};
        let check = ReferenceCheck::Unverifiable("unavailable");
        let err = ensure_no_dangling_references("process", &check).unwrap_err();
        let out: OutputError = err.into();
        assert!(matches!(out.code, ErrorCode::PreconditionFailed));
        assert!(out.message.contains("references_status: unavailable"));
        assert!(out.hint.unwrap().contains("doctor"));
    }

    /// A declaration the language server reads at no position was asked
    /// nothing, which fails closed too, and retrying would ask nothing again.
    #[test]
    fn no_references_guard_refuses_an_unplaced_declaration() {
        use crate::cli::{ErrorCode, OutputError};
        let err = ensure_no_dangling_references("process", &ReferenceCheck::Unplaced).unwrap_err();
        let out: OutputError = err.into();
        assert!(matches!(out.code, ErrorCode::PreconditionFailed));
        assert!(out.message.contains("references_status: unavailable"));
        let hint = out.hint.unwrap();
        assert!(hint.contains("search content"), "{hint}");
        assert!(!hint.contains("retry"), "{hint}");
    }

    /// A zero computed under degraded indexing is a lower bound, not a
    /// verified zero — it refuses, naming the degradation.
    #[test]
    fn no_references_guard_refuses_degraded_zero_as_unverified() {
        use crate::cli::{ErrorCode, OutputError};
        use crate::models::lsp::IndexingDegradation;
        let check = ReferenceCheck::Checked {
            section: Section::new(vec![]).with_indexing(Some(IndexingDegradation::TimedOut)),
            refs_command: "symora refs src/foo.rs:2:1".into(),
        };
        let err = ensure_no_dangling_references("process", &check).unwrap_err();
        let out: OutputError = err.into();
        assert!(matches!(out.code, ErrorCode::PreconditionFailed));
        assert!(out.message.contains("lower bound"));
        assert!(out.message.contains("timed_out"));
        assert!(out.hint.unwrap().contains("retrying may help"));

        let check = ReferenceCheck::Checked {
            section: Section::new(vec![]).with_indexing(Some(IndexingDegradation::Unconfirmed)),
            refs_command: "symora refs src/foo.rs:2:1".into(),
        };
        let out: OutputError = ensure_no_dangling_references("process", &check)
            .unwrap_err()
            .into();
        let hint = out.hint.unwrap();
        assert!(hint.contains("retrying alone does not"), "{hint}");
        assert!(hint.contains("symora refs src/foo.rs:2:1"), "{hint}");
    }

    /// A non-zero count under degradation is stated as "at least" so the
    /// agent never treats it as exhaustive when fixing call sites.
    #[test]
    fn no_references_guard_marks_degraded_count_as_lower_bound() {
        use crate::cli::{ErrorCode, OutputError};
        use crate::models::lsp::IndexingDegradation;
        let check = ReferenceCheck::Checked {
            section: Section::with_total(Vec::new(), 3)
                .with_indexing(Some(IndexingDegradation::TimedOut)),
            refs_command: "symora refs src/foo.rs:2:1".into(),
        };
        let err = ensure_no_dangling_references("process", &check).unwrap_err();
        let out: OutputError = err.into();
        assert!(matches!(out.code, ErrorCode::PreconditionFailed));
        assert!(out.message.contains("at least 3"));
    }

    /// A symbol sharing its first or last line with other code must be
    /// refused by whole-line operations — silently removing a neighbour
    /// is the plausible-but-wrong failure invariant 4 forbids.
    #[test]
    fn line_ownership_guard_refuses_shared_lines() {
        // `fn main() { .. } fn helper() { .. }` on one line: helper's
        // declaration starts at char column 25.
        let content = lines(&[r#"fn main() { helper(); } fn helper() { x(); }"#]);
        let shared = Symbol::new(
            "helper".to_string(),
            SymbolKind::Function,
            Location::full(PathBuf::from("/tmp/a.rs"), 1, 28, 1, 25, 1, 46),
        );
        let span = LineRange { start: 1, end: 1 };
        let err = ensure_exclusive_line_ownership(&content, &shared, &span, &[]).unwrap_err();
        assert!(err.to_string().contains("shares its first line"));

        // Sole occupant of its lines passes (ends at exclusive column 24,
        // one past the 23-char line).
        let solo = lines(&["fn process() { body() }"]);
        let alone = Symbol::new(
            "process".to_string(),
            SymbolKind::Function,
            Location::full(PathBuf::from("/tmp/a.rs"), 1, 4, 1, 1, 1, 24),
        );
        assert!(ensure_exclusive_line_ownership(&solo, &alone, &span, &[]).is_ok());
    }

    #[test]
    fn line_ownership_guard_refuses_trailing_neighbour() {
        let content = lines(&["fn a() {", "} fn b() {}"]);
        let sym = Symbol::new(
            "a".to_string(),
            SymbolKind::Function,
            // ends at exclusive char column 2 on line 2; `fn b() {}` follows
            Location::full(PathBuf::from("/tmp/a.rs"), 1, 4, 1, 1, 2, 2),
        );
        let span = LineRange { start: 1, end: 2 };
        let err = ensure_exclusive_line_ownership(&content, &sym, &span, &[]).unwrap_err();
        assert!(err.to_string().contains("shares its last line"));
    }

    #[test]
    fn destructive_edit_refuses_a_stale_anchor() {
        let sym = sample_symbol(2, 4); // named "process", name on line 2
        // The name line still hosts the symbol — the edit is allowed.
        let fresh = vec![
            "// header".to_string(),
            "fn process() {".to_string(),
            "    work();".to_string(),
            "}".to_string(),
        ];
        assert!(ensure_anchor_not_stale(&sym, &fresh).is_ok());

        // The file shifted since resolution: line 2 now declares a different
        // symbol. A delete/replace there would be unrecoverable, so refuse.
        let stale = vec![
            "// header".to_string(),
            "fn unrelated() {".to_string(),
            "    work();".to_string(),
            "}".to_string(),
        ];
        let err = ensure_anchor_not_stale(&sym, &stale).unwrap_err();
        assert!(err.to_string().contains("no longer on line 2"));

        // The name must match as a WHOLE identifier, not a substring: a stale
        // `process` reappearing only inside `preprocess` / `process_status` is
        // still the wrong symbol and must be refused, never approved.
        for trap in ["fn preprocess() {", "    let process_status = 0;"] {
            let shifted = vec!["// header".to_string(), trap.to_string(), "}".to_string()];
            assert!(
                ensure_anchor_not_stale(&sym, &shifted).is_err(),
                "substring-only match must not pass the guard: {trap}"
            );
        }
    }

    #[test]
    fn anchor_check_uses_the_name_line_not_the_declaration_range_start() {
        // A doc-commented / attributed symbol: its declaration RANGE starts on
        // the leading `///`/`#[…]` lines, but the identifier lives on a later
        // NAME line. The guard must check the name line — anchoring on the
        // range start would never find the name there and would falsely refuse
        // a destructive edit on every documented symbol (the regression this
        // pins). location.line = 3 (the `fn` line); range start = 1 (the doc).
        let sym = Symbol::new(
            "process".to_string(),
            SymbolKind::Function,
            Location::full(PathBuf::from("/tmp/foo.rs"), 3, 4, 1, 1, 5, 1),
        );
        let lines = vec![
            "/// Does the thing.".to_string(),
            "#[inline]".to_string(),
            "fn process() {".to_string(),
            "    work();".to_string(),
            "}".to_string(),
        ];
        assert!(
            ensure_anchor_not_stale(&sym, &lines).is_ok(),
            "a documented symbol whose name is on its name line must not be refused"
        );
    }

    #[test]
    fn anchor_check_skips_names_without_a_textual_anchor() {
        // An anonymous / non-identifier name has no name to find in the source,
        // so the guard must never produce a false refusal for it.
        let sym = Symbol::new(
            "<closure>".to_string(),
            SymbolKind::Function,
            Location::full(PathBuf::from("/tmp/foo.rs"), 1, 1, 1, 1, 1, 1),
        );
        assert!(ensure_anchor_not_stale(&sym, &["let f = || {};".to_string()]).is_ok());
    }
}
