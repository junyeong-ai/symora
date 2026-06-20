//! The single mutation surface. Every command that splices source text
//! lives here, sharing one resolution path (root-validated), one
//! validation gate, one splice core, one preview format, and one typed
//! output (`EditOutput`).
//!
//! Symbol-targeted subcommands (`replace-body`, `insert-before`,
//! `insert-after`) operate on whole lines of the symbol's full
//! declaration span — the body the agent supplies is taken verbatim,
//! indentation included. `replace` and `pattern` are character-precise;
//! all columns at this boundary are 1-indexed *character* counts (the
//! AST layer's byte columns are converted before they reach the core).

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

use crate::app::App;
use crate::cli::ParsedLocation;
use crate::cli::response::{EditOutput, LineRange, Section};
use crate::models::lsp::FindSymbolsOptions;
use crate::models::symbol::{Language, Symbol};
use crate::utils::char_to_byte_index;

/// Maximum file size for editing (100MB)
const MAX_EDIT_FILE_SIZE: u64 = 100 * 1024 * 1024;

#[derive(Args, Debug)]
pub struct EditArgs {
    #[command(subcommand)]
    pub command: EditCommand,
}

#[derive(Subcommand, Debug)]
pub enum EditCommand {
    /// Replace a symbol's ENTIRE definition span (whole lines, signature
    /// through closing brace) — pass the complete definition, not just
    /// the inner code. For a raw character range use `replace`.
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
    match command {
        EditCommand::ReplaceBody {
            target,
            symbol,
            body,
            dry_run,
            with_diagnostics,
            verify_callers,
        } => {
            let body = read_payload(&body)?;
            let (file, sym) = resolve_symbol(app, &target, symbol).await?;
            symbol_edit(
                app,
                "replace_body",
                &file,
                &sym,
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
            let code = read_payload(&code)?;
            let (file, sym) = resolve_symbol(app, &target, symbol).await?;
            symbol_edit(
                app,
                "insert_before",
                &file,
                &sym,
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
            let code = read_payload(&code)?;
            let (file, sym) = resolve_symbol(app, &target, symbol).await?;
            symbol_edit(
                app,
                "insert_after",
                &file,
                &sym,
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
            let (file, sym) = resolve_symbol(app, &target, symbol).await?;
            delete_symbol(
                app,
                &file,
                &sym,
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
            let text = read_payload(&text)?;
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
        } => {
            let text = read_payload(&text)?;
            pattern_edit(app, &file, &pattern, &lang, &text, &index, dry_run).await
        }
    }
}

/// Shared tail for symbol-targeted line edits: resolve the span with the
/// stale-range guard, splice, emit one `EditOutput`, refresh the index.
#[allow(clippy::too_many_arguments)]
async fn symbol_edit(
    app: &App,
    operation: &'static str,
    file: &Path,
    symbol: &Symbol,
    dry_run: bool,
    with_diagnostics: bool,
    verify_callers: bool,
    make_splice: impl FnOnce(&LineRange) -> LineSplice,
) -> Result<()> {
    let doc = FileDocument::load(file, dry_run)?;
    let span = symbol_line_span(symbol, doc.lines.len())?;
    let splice = make_splice(&span);
    if splice.removed > 0 {
        ensure_anchor_not_stale(symbol, &doc.lines)?;
        ensure_exclusive_line_ownership(&doc.lines, symbol, &span)?;
    }
    // Resolve the caller files BEFORE the edit lands: editing a symbol does not
    // move which files reference it, and resolving against the pre-edit state
    // avoids any post-edit position drift. The diagnostics for those files are
    // pulled AFTER the write, so they reflect the edit's effect.
    let caller_files = if verify_callers && !dry_run {
        Some(collect_caller_files(app, file, symbol).await)
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
/// symbol's first line has non-whitespace before its declaration start,
/// or its last line has non-whitespace after its declared end column,
/// the operation is refused with a character-precise alternative —
/// never a silent over-splice.
fn ensure_exclusive_line_ownership(
    lines: &[String],
    symbol: &Symbol,
    span: &LineRange,
) -> Result<()> {
    let shared = |what: &str| {
        anyhow::Error::new(
            crate::cli::OutputError::unsupported(format!(
                "Symbol '{}' shares its {what} line with other code; \
                 whole-line edits would remove it",
                symbol.name,
            ))
            .with_hint("Use `edit replace` for character-precise control"),
        )
    };

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
    file: &Path,
    symbol: &Symbol,
    dry_run: bool,
    with_diagnostics: bool,
    expect_no_references: bool,
) -> Result<()> {
    let doc = FileDocument::load(file, dry_run)?;
    let span = symbol_line_span(symbol, doc.lines.len())?;
    ensure_anchor_not_stale(symbol, &doc.lines)?;
    ensure_exclusive_line_ownership(&doc.lines, symbol, &span)?;

    let check = check_dangling_references(app, file, symbol, &span).await;
    if expect_no_references {
        let refs_command = format!(
            "symora refs {}:{}:{}",
            app.output.relative_path(file),
            symbol.location.line,
            symbol.location.column,
        );
        ensure_no_dangling_references(&symbol.name, &refs_command, &check)?;
    }

    let splice = LineSplice {
        at: span.start as usize - 1,
        removed: (span.end - span.start + 1) as usize,
        new_lines: Vec::new(),
    };
    let target = Some((symbol.path().to_string(), symbol.kind.to_string()));
    let mut output = doc.commit(app, "delete", splice, span, target, dry_run)?;
    match check {
        ReferenceCheck::Checked(section) => output.dangling_references = Some(section),
        ReferenceCheck::Unverifiable(status) => output.references_status = Some(status),
    }
    output.diagnostics = pull_diagnostics(app, file, dry_run, with_diagnostics).await;
    app.output.print_success(output);
    finish(app, file, dry_run).await;
    Ok(())
}

/// Cap on inline dangling-reference listings; the full set stays one
/// `find_references` call away and the section says it was truncated.
const DELETE_REFS_DISPLAY_LIMIT: usize = 50;

/// Outcome of the pre-delete reference lookup. The two variants map 1:1
/// onto `EditOutput`'s exactly-one-of `dangling_references` /
/// `references_status` fields, so the presence rule is enforced at the
/// producer's type rather than by convention.
enum ReferenceCheck {
    /// The lookup ran; the section lists span-filtered dangling
    /// references and carries the `indexing` marker when the count is a
    /// lower bound.
    Checked(Section<crate::cli::response::LocationOutput>),
    /// The lookup could not run; the status string is the
    /// `references_status` value emitted on `EditOutput`
    /// (`"unsupported"` | `"unavailable"`).
    Unverifiable(&'static str),
}

/// References at the symbol's *name* position (not the declaration
/// start the splice uses — nothing references the `pub` keyword) that
/// live outside the deleted span. Honest per invariant 4: when the
/// language can't answer, return a status instead of implying "none".
async fn check_dangling_references(
    app: &App,
    file: &Path,
    symbol: &Symbol,
    span: &LineRange,
) -> ReferenceCheck {
    use crate::cli::response::LocationOutput;
    use crate::infra::lsp::capabilities::{LspFeature, SupportLevel, get_support_level};

    if get_support_level(Language::from_path(file), LspFeature::FindReferences)
        == SupportLevel::None
    {
        return ReferenceCheck::Unverifiable("unsupported");
    }

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
    if total > DELETE_REFS_DISPLAY_LIMIT {
        section = section.with_next_commands(vec![format!(
            "symora refs {}:{}:{}",
            app.output.relative_path(file),
            symbol.location.line,
            symbol.location.column,
        )]);
    }
    ReferenceCheck::Checked(section)
}

/// The no-references precondition, fail-closed: the ONLY state that
/// passes is a completed check with zero dangling references and no
/// indexing degradation. A non-answer or a documented lower bound must
/// never read as "confirmed reference-free" (invariant 4) — every other
/// state refuses, naming its exact reason in the message and a working
/// alternative in the hint.
fn ensure_no_dangling_references(
    symbol_name: &str,
    refs_command: &str,
    check: &ReferenceCheck,
) -> Result<()> {
    use crate::models::lsp::IndexingDegradation;

    // Exhaustive on purpose: a future degradation variant is a compile
    // error here, never a silently wrong marker. The string matches the
    // variant's snake_case wire form.
    fn marker(degradation: IndexingDegradation) -> &'static str {
        match degradation {
            IndexingDegradation::TimedOut => "timed_out",
        }
    }

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
                     sites manually (try: symora search content '{symbol_name}') and \
                     rerun without --expect-no-references."
                )
            } else {
                "The reference lookup failed; retry once, check 'symora doctor', or \
                 verify manually and rerun without --expect-no-references."
                    .to_string()
            };
            anyhow::bail!(refuse(message, hint))
        }
        ReferenceCheck::Checked(section) if section.count > 0 => {
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
                    marker(degradation),
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
        ReferenceCheck::Checked(section) => match section.indexing {
            Some(degradation) => anyhow::bail!(refuse(
                format!(
                    "Delete of '{symbol_name}' refused: the reference count of 0 is a \
                     lower bound under degraded indexing (indexing: {}), which does \
                     not verify the no-references precondition",
                    marker(degradation),
                ),
                "Wait for the language server to finish indexing (check 'symora \
                 status'), then retry; or verify manually and rerun without \
                 --expect-no-references."
                    .to_string(),
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
            count: report.items.len(),
            items: report.items.iter().map(DiagnosticOutput::from).collect(),
        },
        Err(e) => {
            tracing::warn!("Diagnostics pull failed for {}: {e}", file.display());
            EditDiagnostics {
                status: "unavailable",
                count: 0,
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
    indexing: Option<crate::models::lsp::IndexingDegradation>,
    status: Option<&'static str>,
}

/// Resolve the distinct files that reference `symbol`, EXCLUDING the edited file
/// — its same-file callers are covered by the edit's own edited-file diagnostics
/// (which verify-callers forces on), so listing it here too would double-diagnose
/// it. Capped and deterministic. `status` is set only when the reference lookup
/// could not run — never paired with files.
async fn collect_caller_files(app: &App, file: &Path, symbol: &Symbol) -> CallerFiles {
    use crate::infra::lsp::capabilities::{LspFeature, SupportLevel, get_support_level};

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

// ---------------------------------------------------------------------------
// Splice core
// ---------------------------------------------------------------------------

/// One planned, contiguous content change in line units: `removed`
/// original lines starting at `at` are replaced by `new_lines`. Inserts
/// are the `removed == 0` case. Every subcommand reduces to this, which
/// is what keeps validation, preview, and output uniform.
#[derive(Debug)]
struct LineSplice {
    /// 0-indexed first affected line.
    at: usize,
    /// Number of original lines replaced.
    removed: usize,
    new_lines: Vec<String>,
}

impl LineSplice {
    fn apply(&self, lines: &[String]) -> Vec<String> {
        let mut out = Vec::with_capacity(lines.len() - self.removed + self.new_lines.len());
        out.extend_from_slice(&lines[..self.at]);
        out.extend(self.new_lines.iter().cloned());
        out.extend_from_slice(&lines[self.at + self.removed..]);
        out
    }

    /// Exact unified hunk derived from the splice itself — no diff walk,
    /// so an insert can never smear change markers over untouched lines.
    fn unified_hunk(&self, lines: &[String]) -> String {
        let mut out = format!(
            "@@ -{},{} +{},{} @@\n",
            self.at + 1,
            self.removed,
            self.at + 1,
            self.new_lines.len(),
        );
        for line in &lines[self.at..self.at + self.removed] {
            out.push('-');
            out.push_str(line);
            out.push('\n');
        }
        for line in &self.new_lines {
            out.push('+');
            out.push_str(line);
            out.push('\n');
        }
        out
    }
}

/// A file loaded for editing: validated up front, split into lines once,
/// rendered back with its original line-ending style and trailing-newline
/// behaviour preserved.
struct FileDocument {
    file: PathBuf,
    content: String,
    lines: Vec<String>,
    eol: &'static str,
}

impl FileDocument {
    /// Existence and the size cap are always enforced; writability only
    /// when the edit will actually hit disk.
    fn load(file: &Path, dry_run: bool) -> Result<Self> {
        validate_file_for_edit(file, !dry_run)?;
        let content = fs::read_to_string(file)
            .with_context(|| format!("Failed to read file: {}", file.display()))?;
        let lines: Vec<String> = content.lines().map(String::from).collect();
        let eol = detect_line_ending(&content);
        Ok(Self {
            file: file.to_path_buf(),
            content,
            lines,
            eol,
        })
    }

    fn render(&self, lines: &[String]) -> String {
        let mut out = lines.join(self.eol);
        if self.content.ends_with('\n') && !out.is_empty() {
            out.push_str(self.eol);
        }
        out
    }

    /// Apply one splice: write (or preview) and build the typed output.
    fn commit(
        &self,
        app: &App,
        operation: &'static str,
        splice: LineSplice,
        span: LineRange,
        target: Option<(String, String)>,
        dry_run: bool,
    ) -> Result<EditOutput> {
        if splice.at + splice.removed > self.lines.len() {
            anyhow::bail!(crate::cli::CliInputError::LineOutOfRange {
                line: (splice.at + splice.removed) as u32,
                total: self.lines.len(),
            });
        }

        let new_content = self.render(&splice.apply(&self.lines));
        let bytes_changed = new_content.len() as i64 - self.content.len() as i64;

        let preview = if dry_run {
            Some(splice.unified_hunk(&self.lines))
        } else {
            atomic_write(&self.file, &new_content)?;
            None
        };

        let (target_symbol, target_kind) = match target {
            Some((s, k)) => (Some(s), Some(k)),
            None => (None, None),
        };

        Ok(EditOutput {
            operation,
            file: app.output.relative_path(&self.file),
            target_symbol,
            target_kind,
            lines: span,
            bytes_changed,
            dry_run,
            preview,
            dangling_references: None,
            references_status: None,
            diagnostics: None,
            caller_verification: None,
        })
    }
}

/// Reduce a 1-indexed character range to a `LineSplice`. `end_col` is
/// exclusive; `None` means "through the end of `end_line`".
///
/// Positions are validated, never clamped: a column past the line end is
/// the caller addressing content that isn't there, and silently snapping
/// it to EOL would splice somewhere the caller didn't ask for.
fn char_splice(
    lines: &[String],
    start_line: u32,
    start_col: u32,
    end_line: u32,
    end_col: Option<u32>,
    text: &str,
    expect: Option<&str>,
) -> Result<LineSplice> {
    // `--expect` is a staleness precondition: confirm the live text first,
    // bounds-tolerantly, so geometry that no longer exists (the file shrank
    // against the revision the caller read) surfaces as a `Conflict` to
    // re-read and retry — not as an `InvalidArgument` the agent won't retry.
    if let Some(expected) = expect {
        confirm_expected_region(lines, start_line, start_col, end_line, end_col, expected)?;
    }

    // An empty file has exactly one addressable position: 1:1.
    if lines.is_empty() {
        if start_line == 1 && end_line == 1 && start_col == 1 && end_col.unwrap_or(1) == 1 {
            return Ok(LineSplice {
                at: 0,
                removed: 0,
                new_lines: split_merged_lines(text),
            });
        }
        anyhow::bail!(crate::cli::CliInputError::LineOutOfRange {
            line: start_line,
            total: 0,
        });
    }

    let start_idx = (start_line.saturating_sub(1)) as usize;
    let end_idx = (end_line.saturating_sub(1)) as usize;
    for (label, idx) in [(start_line, start_idx), (end_line, end_idx)] {
        if idx >= lines.len() {
            anyhow::bail!(crate::cli::CliInputError::LineOutOfRange {
                line: label,
                total: lines.len(),
            });
        }
    }
    if end_idx < start_idx {
        anyhow::bail!(invalid_range(format!(
            "End line {end_line} precedes start line {start_line}"
        )));
    }

    let start_line_str = &lines[start_idx];
    ensure_column_in_line(start_col, start_line_str, start_line)?;
    let start_byte = char_to_byte_index(start_line_str, (start_col.saturating_sub(1)) as usize);
    let prefix = &start_line_str[..start_byte];

    let end_line_str = &lines[end_idx];
    let end_chars = match end_col {
        Some(col) => {
            ensure_column_in_line(col, end_line_str, end_line)?;
            if start_idx == end_idx && col < start_col {
                anyhow::bail!(invalid_range(format!(
                    "End column {col} precedes start column {start_col} on line {start_line}"
                )));
            }
            (col.saturating_sub(1)) as usize
        }
        None => end_line_str.chars().count(),
    };
    let end_byte = char_to_byte_index(end_line_str, end_chars);
    let suffix = &end_line_str[end_byte..];

    Ok(LineSplice {
        at: start_idx,
        removed: end_idx - start_idx + 1,
        new_lines: split_merged_lines(&format!("{prefix}{text}{suffix}")),
    })
}

/// Reconstruct the live text a character range covers, exactly as it sits
/// on disk. `FileDocument` stores lines terminator-stripped, so multi-line
/// spans join with `\n` — the canonical form `--expect` is normalized to.
fn spliced_region(
    lines: &[String],
    start_idx: usize,
    start_byte: usize,
    end_idx: usize,
    end_byte: usize,
) -> String {
    if start_idx == end_idx {
        return lines[start_idx][start_byte..end_byte].to_string();
    }
    let mut region = String::new();
    region.push_str(&lines[start_idx][start_byte..]);
    for line in &lines[start_idx + 1..end_idx] {
        region.push('\n');
        region.push_str(line);
    }
    region.push('\n');
    region.push_str(&lines[end_idx][..end_byte]);
    region
}

/// Bounds-tolerant region read: the live text the range covers, or `None`
/// when the range no longer fits the file (a line/column out of range, or an
/// inverted range). Used only by the `--expect` precondition, where being
/// unable to address the region means the file changed under the caller.
fn live_region(
    lines: &[String],
    start_line: u32,
    start_col: u32,
    end_line: u32,
    end_col: Option<u32>,
) -> Option<String> {
    if lines.is_empty() {
        // The only addressable region of an empty file is 1:1, spanning "".
        let origin =
            start_line == 1 && end_line == 1 && start_col == 1 && end_col.unwrap_or(1) == 1;
        return origin.then(String::new);
    }
    let start_idx = start_line.saturating_sub(1) as usize;
    let end_idx = end_line.saturating_sub(1) as usize;
    if start_idx >= lines.len() || end_idx >= lines.len() || end_idx < start_idx {
        return None;
    }
    let start_line_str = &lines[start_idx];
    let end_line_str = &lines[end_idx];
    let start_chars = start_col.saturating_sub(1) as usize;
    if start_chars > start_line_str.chars().count() {
        return None;
    }
    let end_chars = match end_col {
        Some(col) => {
            let chars = col.saturating_sub(1) as usize;
            if chars > end_line_str.chars().count() || (start_idx == end_idx && col < start_col) {
                return None;
            }
            chars
        }
        None => end_line_str.chars().count(),
    };
    let start_byte = char_to_byte_index(start_line_str, start_chars);
    let end_byte = char_to_byte_index(end_line_str, end_chars);
    Some(spliced_region(
        lines, start_idx, start_byte, end_idx, end_byte,
    ))
}

/// Confirm the live text spanned by the range equals `expected`, treating
/// *any* inability to read exactly that region — out-of-range geometry from a
/// file that shrank, as well as a content mismatch — as a stale-revision
/// `Conflict`. The comparison is exact, tolerating only `\r\n` vs `\n` (line
/// terminators carry no meaning in the splice model — lines are stored
/// stripped, so a CRLF file and an LF file address the same text); indentation
/// and every other byte must match, so a normalized compare can never
/// silently accept a different edit.
fn confirm_expected_region(
    lines: &[String],
    start_line: u32,
    start_col: u32,
    end_line: u32,
    end_col: Option<u32>,
    expected: &str,
) -> Result<()> {
    let want = normalize_newlines(expected);

    let matched = match live_region(lines, start_line, start_col, end_line, end_col) {
        Some(actual) => want == actual,
        None => false,
    };
    if matched {
        return Ok(());
    }

    let end = match end_col {
        Some(col) => col.to_string(),
        None => "eol".to_string(),
    };
    anyhow::bail!(stale_revision(format!(
        "Live text at {start_line}:{start_col}..{end_line}:{end} does not match --expect; \
         the file changed against a different revision — retry"
    )))
}

fn normalize_newlines(text: &str) -> String {
    text.split('\n')
        .map(|segment| segment.strip_suffix('\r').unwrap_or(segment))
        .collect::<Vec<_>>()
        .join("\n")
}

fn invalid_range(message: String) -> anyhow::Error {
    anyhow::Error::new(crate::cli::OutputError::invalid(message))
}

/// A computed or asserted edit range no longer matches the on-disk file —
/// the analysis (LSP range, AST match, or an `--expect` assertion) ran
/// against a different revision than the bytes on disk. Surfaced as
/// `ErrorCode::Conflict` so an agent branches on it to re-read and retry
/// instead of treating a recoverable staleness as an internal failure.
fn stale_revision(message: String) -> anyhow::Error {
    anyhow::Error::new(crate::cli::OutputError::conflict(message))
}

fn symbol_not_found(pattern: &str, file: &str) -> anyhow::Error {
    anyhow::Error::new(
        crate::cli::OutputError::not_found(format!("Symbol not found: {pattern} in {file}"))
            .with_hint(format!("List symbol paths with 'symora symbols {file}'")),
    )
}

/// Candidates rendered inline in the ambiguity hint before `+N more`
/// takes over — the message always carries the true total, and the full
/// set stays one `symora symbols <file>` call away.
const AMBIGUOUS_CANDIDATE_DISPLAY_LIMIT: usize = 5;

/// A `--symbol` pattern matching several symbols — a bare name or wildcard
/// hitting siblings, or same-named parents sharing a child path — is an
/// under-specified target the agent fixes by re-addressing; a
/// `file:line[:col]` target is the honest disambiguator, so the hint routes
/// there.
fn ambiguous_symbol_path(pattern: &str, candidates: &[&Symbol], file: &str) -> anyhow::Error {
    let mut listing = candidates
        .iter()
        .take(AMBIGUOUS_CANDIDATE_DISPLAY_LIMIT)
        .map(|s| format!("{} ({}) line {}", s.path(), s.kind, s.location.line))
        .collect::<Vec<_>>()
        .join(", ");
    if candidates.len() > AMBIGUOUS_CANDIDATE_DISPLAY_LIMIT {
        listing.push_str(&format!(
            ", +{} more",
            candidates.len() - AMBIGUOUS_CANDIDATE_DISPLAY_LIMIT
        ));
    }
    anyhow::Error::new(
        crate::cli::OutputError::invalid(format!(
            "Symbol path '{pattern}' matches {} symbols in {file}",
            candidates.len()
        ))
        .with_hint(format!(
            "Candidates: {listing}. Target one by file:line[:col] instead."
        )),
    )
}

fn conflicting_addressing(target: &str) -> anyhow::Error {
    anyhow::Error::new(
        crate::cli::OutputError::invalid(format!(
            "Target '{target}' is a location and --symbol was also given; pass exactly one"
        ))
        .with_hint("Use file:line[:col] alone, or a plain file path with --symbol"),
    )
}

/// Valid columns run 1..=chars+1 — one past the last character is the
/// zero-width position at EOL.
fn ensure_column_in_line(col: u32, line: &str, line_no: u32) -> Result<()> {
    let chars = line.chars().count();
    if (col as usize) > chars + 1 {
        anyhow::bail!(invalid_range(format!(
            "Column {col} exceeds line {line_no} length ({chars} characters)"
        )));
    }
    Ok(())
}

/// Split merged splice content into lines, normalizing CRLF in the
/// replacement text — `render` re-joins with the file's own line
/// ending, so a carried `\r` would corrupt CRLF files into `\r\r\n`.
fn split_merged_lines(text: &str) -> Vec<String> {
    text.split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
        .collect()
}

fn to_lines(text: &str) -> Vec<String> {
    text.lines().map(String::from).collect()
}

fn read_payload(value: &str) -> Result<String> {
    if value == "-" {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        Ok(buf)
    } else {
        Ok(value.to_string())
    }
}

/// Validate a file for editing. Existence and the size cap always apply;
/// `for_write` adds the writability probe.
fn validate_file_for_edit(path: &Path, for_write: bool) -> Result<()> {
    use crate::cli::CliInputError;

    if !path.exists() {
        anyhow::bail!(CliInputError::FileNotFound(path.to_path_buf()));
    }

    let metadata = fs::metadata(path).context("Failed to read file metadata")?;

    if metadata.len() > MAX_EDIT_FILE_SIZE {
        anyhow::bail!(CliInputError::FileTooLarge {
            size_mb: metadata.len() / (1024 * 1024),
            limit_mb: MAX_EDIT_FILE_SIZE / (1024 * 1024),
        });
    }

    if for_write && fs::OpenOptions::new().write(true).open(path).is_err() {
        anyhow::bail!(CliInputError::FileNotWritable(path.to_path_buf()));
    }

    Ok(())
}

/// Detect the line ending style used in content
fn detect_line_ending(content: &str) -> &'static str {
    if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

// ---------------------------------------------------------------------------
// Target resolution
// ---------------------------------------------------------------------------

/// Resolve file path from target string, handling relative paths.
/// Canonicalizes the result and validates it is within the project root.
fn resolve_file_path(app: &App, target: &str) -> Result<PathBuf> {
    let path = Path::new(target);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        app.root().join(path)
    };
    // Validate existence before canonicalizing: a missing path makes
    // `canonicalize` fail with an opaque io error that reads as an internal
    // fault, when it is really a `not_found` the caller can act on.
    if !resolved.is_file() {
        anyhow::bail!(crate::cli::CliInputError::FileNotFound(resolved));
    }
    let canonical = resolved
        .canonicalize()
        .with_context(|| format!("Cannot resolve path: {}", resolved.display()))?;
    let root = app
        .root()
        .canonicalize()
        .unwrap_or_else(|_| app.root().to_path_buf());
    if !canonical.starts_with(&root) {
        anyhow::bail!(crate::cli::CliInputError::PathOutsideProject(canonical));
    }
    Ok(canonical)
}

/// Resolve a `--symbol` path pattern in a file to the one symbol it names,
/// through the same `matches_path` predicate every `--symbol` surface
/// resolves through (bare last-component, `/`-anchored suffix, `*` wildcard,
/// or a leading-`/` exact path). The async wrapper fetches the file's
/// symbols; `unique_symbol_by_path` owns the dispatch, so the destructive
/// resolution stays unit-tested without an LSP round-trip.
async fn find_symbol_by_path(app: &App, file: &Path, pattern: &str) -> Result<Symbol> {
    let mut symbols = app
        .lsp
        .find_symbols(file, FindSymbolsOptions::default().with_depth(10))
        .await?;
    Symbol::compute_paths_for_all(&mut symbols);
    unique_symbol_by_path(&symbols, pattern, &app.output.relative_path(file))
}

/// Pick the one symbol a flexible `--symbol` pattern names: exactly one
/// match resolves; zero is a structured not-found; several is a structured
/// ambiguity listing the candidates — silently editing an arbitrary match
/// would be plausible-but-wrong. Pure (paths pre-computed by the caller),
/// so the destructive dispatch is directly testable.
fn unique_symbol_by_path(symbols: &[Symbol], pattern: &str, file_display: &str) -> Result<Symbol> {
    match Symbol::filter_by_path(symbols, pattern).as_slice() {
        [] => Err(symbol_not_found(pattern, file_display)),
        [only] => Ok((*only).clone()),
        many => Err(ambiguous_symbol_path(pattern, many, file_display)),
    }
}

/// Find the symbol an edit target addresses, through the shared
/// resolution functions every surface uses (`cli::utils::symbol_nav`).
/// With a column, resolution is position-precise; without one, the line's
/// own declaration is preferred over the enclosing block
/// (`line_addressed_symbol`). Ambiguity and misses become structured
/// errors the agent can act on — guessing would edit a sibling the caller
/// didn't address.
async fn find_symbol_at_location(
    app: &App,
    file: &Path,
    line: u32,
    column: Option<u32>,
) -> Result<Symbol> {
    use crate::cli::utils::{SymbolResolution, column_addressed_symbol, line_addressed_symbol};

    let symbols = app
        .lsp
        .find_symbols(file, FindSymbolsOptions::default())
        .await?;

    let resolution = match column {
        Some(col) => column_addressed_symbol(&symbols, line, col),
        None => line_addressed_symbol(&symbols, line),
    };

    match resolution {
        SymbolResolution::Match(symbol) => Ok(symbol.clone()),
        SymbolResolution::NotFound => {
            let file_display = app.output.relative_path(file);
            Err(anyhow::Error::new(
                crate::cli::OutputError::not_found(format!(
                    "No symbol found at line {line} in {file_display}"
                ))
                .with_hint(format!(
                    "List symbol paths with 'symora symbols {file_display}'"
                )),
            ))
        }
        SymbolResolution::Ambiguous(declared) => {
            let names: Vec<&str> = declared.iter().map(|s| s.name.as_str()).collect();
            let message = match column {
                Some(col) => format!(
                    "Line {line} declares multiple symbols ({}); column {col} \
                     matches none of them",
                    names.join(", "),
                ),
                None => format!(
                    "Line {line} declares multiple symbols ({})",
                    names.join(", "),
                ),
            };
            Err(anyhow::Error::new(
                crate::cli::OutputError::invalid(message)
                    .with_hint("Pass the exact column of the symbol to edit (file:line:col)"),
            ))
        }
    }
}

/// Resolve full symbol from target. Auto-detects location format
/// (`file:line[:col]`) vs file path (requires --symbol); passing both
/// addressing modes at once is refused rather than silently picking one.
async fn resolve_symbol(
    app: &App,
    target: &str,
    symbol_path: Option<String>,
) -> Result<(PathBuf, Symbol)> {
    // Location mode: find symbol at position. An omitted column is its
    // own addressing mode (line-declared symbol first), not column 1.
    if ParsedLocation::is_location_format(target) {
        if symbol_path.is_some() {
            anyhow::bail!(conflicting_addressing(target));
        }
        let loc = ParsedLocation::parse(target)?.to_absolute_with_root(Some(app.root()))?;
        let column = loc.column_explicit.then_some(loc.column);
        let symbol = find_symbol_at_location(app, &loc.file, loc.line, column).await?;
        return Ok((loc.file, symbol));
    }

    // Symbol mode: --symbol is required
    let pattern = symbol_path.ok_or_else(|| {
        anyhow::Error::new(
            crate::cli::OutputError::invalid("--symbol is required when target is a file path")
                .with_hint(
                    "Pass file:line[:col] for position addressing, or a file path plus --symbol",
                ),
        )
    })?;

    let file = resolve_file_path(app, target)?;
    let symbol = find_symbol_by_path(app, &file, &pattern).await?;
    Ok((file, symbol))
}

/// The symbol's full declaration span (1-indexed, inclusive), with the
/// stale-range guard: an LSP range past EOF means the server analyzed an
/// older revision, and splicing by it would corrupt the file.
fn symbol_line_span(symbol: &Symbol, total_lines: usize) -> Result<LineRange> {
    let (start, _) = symbol.location.effective_start();
    let start = start.max(1);
    let end = symbol
        .location
        .end_line
        .unwrap_or(symbol.location.line)
        .max(start);
    // `> total_lines`, not `total_lines.max(1)`: an emptied file has 0 lines, so
    // a stale line-1 anchor (end == 1 > 0) must fail closed here rather than
    // reach the splice and surface as a misleading out-of-range error. A symbol
    // can never legitimately resolve into a 0-line file.
    if (end as usize) > total_lines {
        anyhow::bail!(stale_revision(format!(
            "Symbol end line {end} exceeds file length {total_lines}; LSP range is stale, retry"
        )));
    }
    Ok(LineRange { start, end })
}

// ---------------------------------------------------------------------------
// Pattern edit (tree-sitter)
// ---------------------------------------------------------------------------

/// AST pattern edit: a list operation, one `EditOutput` per affected
/// match, wrapped in the shared `Section` list shape.
async fn pattern_edit(
    app: &App,
    file: &str,
    pattern: &str,
    lang: &str,
    text: &str,
    index: &str,
    dry_run: bool,
) -> Result<()> {
    let language = Language::parse_or_default(lang);
    if language == Language::Unknown {
        anyhow::bail!(
            "Unsupported language: {}. Run 'symora search nodes --list' for supported languages.",
            lang
        );
    }

    let abs_path = resolve_file_path(app, file)?;
    let doc = FileDocument::load(&abs_path, dry_run)?;

    let matches = app
        .ast
        .query(pattern, language, std::slice::from_ref(&abs_path))
        .await
        .map_err(|e| anyhow::anyhow!("AST query failed: {}", e))?;

    let replace_all = index.eq_ignore_ascii_case("all");
    let target_index: Option<usize> = if replace_all {
        None
    } else {
        Some(
            index
                .parse()
                .context("Invalid index: must be a number or 'all'")?,
        )
    };
    if let Some(idx) = target_index
        && !matches.is_empty()
        && idx >= matches.len()
    {
        anyhow::bail!(
            "Index {} out of range. Found {} matches.",
            idx,
            matches.len()
        );
    }

    let selected: Vec<_> = matches
        .iter()
        .enumerate()
        .filter(|(i, _)| replace_all || target_index == Some(*i))
        .collect();

    if selected.is_empty() {
        let section: Section<EditOutput> =
            Section::new(vec![]).with_hints(vec![format!("No matches for pattern in {file}")]);
        app.output.print_success(section);
        return Ok(());
    }

    // Each match reduces to a char splice against the file state it is
    // applied to. Bottom-up order keeps earlier (higher) coordinates
    // valid while later (lower) matches are spliced first.
    let mut ordered = selected.clone();
    ordered.sort_by(|a, b| {
        (b.1.start_line, b.1.start_column).cmp(&(a.1.start_line, a.1.start_column))
    });

    let mut outputs = Vec::with_capacity(ordered.len());
    let mut working = doc.lines.clone();
    for (_, m) in &ordered {
        let start_idx = (m.start_line.saturating_sub(1)) as usize;
        let end_idx = (m.end_line.saturating_sub(1)) as usize;
        if start_idx >= working.len() || end_idx >= working.len() {
            anyhow::bail!(stale_revision(format!(
                "Match span {}..{} exceeds file length {}; the AST index is stale, retry",
                m.start_line,
                m.end_line,
                working.len()
            )));
        }
        // AstMatch columns are already 1-indexed character columns (the JSON
        // contract), exactly what the splice core speaks — pass them straight
        // through.
        let splice = char_splice(
            &working,
            m.start_line,
            m.start_column,
            m.end_line,
            Some(m.end_column),
            text,
            None,
        )?;

        let old_len: usize = region_len(&working, &splice, doc.eol);
        let new_len: usize = splice.new_lines.join(doc.eol).len();
        outputs.push(EditOutput {
            operation: "pattern",
            file: app.output.relative_path(&abs_path),
            target_symbol: None,
            target_kind: None,
            lines: LineRange {
                start: m.start_line,
                end: m.end_line,
            },
            bytes_changed: new_len as i64 - old_len as i64,
            dry_run,
            preview: dry_run.then(|| splice.unified_hunk(&working)),
            dangling_references: None,
            references_status: None,
            diagnostics: None,
            caller_verification: None,
        });

        if !dry_run {
            working = splice.apply(&working);
        }
    }
    // Emit in original (top-down) match order.
    outputs.reverse();

    if !dry_run {
        let new_content = doc.render(&working);
        atomic_write(&abs_path, &new_content)?;
    }

    let mut section = Section::with_total(outputs, matches.len());
    if replace_all || matches.len() > 1 {
        section = section.with_hints(vec![format!(
            "{} of {} matches {}",
            selected.len(),
            matches.len(),
            if dry_run {
                "would be replaced"
            } else {
                "replaced"
            },
        )]);
    }
    app.output.print_success(section);
    finish(app, &abs_path, dry_run).await;
    Ok(())
}

fn region_len(lines: &[String], splice: &LineSplice, eol: &str) -> usize {
    let region = &lines[splice.at..splice.at + splice.removed];
    let newline_bytes = eol.len() * region.len().saturating_sub(1);
    region.iter().map(String::len).sum::<usize>() + newline_bytes
}

// ---------------------------------------------------------------------------
// Atomic write
// ---------------------------------------------------------------------------

/// Write `content` to `target` atomically: stage into a dotfile in the
/// target's directory, fsync, carry the target's permissions over, rename
/// into place, then best-effort fsync the directory to make the rename
/// durable. A crash at any point can never leave a truncated source file —
/// the worst case is an orphaned staging file, which the drop guard removes
/// on every in-process failure path. Staging next to the target (not in a
/// temp dir) keeps the rename on one filesystem, where it is atomic.
pub(crate) fn atomic_write(target: &Path, content: &str) -> Result<()> {
    // Renaming over a symlink would replace the link itself, not the file it
    // points to — resolve first so the edit lands in the real file.
    let target = target
        .canonicalize()
        .unwrap_or_else(|_| target.to_path_buf());
    let parent = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Target has no parent: {}", target.display()))?;
    let staging = parent.join(format!(
        ".{}.symora-edit.{}",
        target
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("edit"),
        std::process::id()
    ));

    let mut guard = StagingFile::new(&staging);
    {
        // `create_new` (O_EXCL) won't follow or open an existing path at the
        // staging name, so a symlink planted there can't redirect the write
        // into another file. If a same-pid staging file is somehow already
        // present, it fails loudly here instead of being silently reused.
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)
            .with_context(|| format!("Failed to stage write: {}", staging.display()))?;
        file.write_all(content.as_bytes())
            .with_context(|| format!("Failed to stage write: {}", staging.display()))?;
        file.sync_all()
            .with_context(|| format!("Failed to stage write: {}", staging.display()))?;
    }

    // The staging file was created with default permissions; the target's
    // mode (an executable script, say) must survive the rename.
    if let Ok(meta) = fs::metadata(&target) {
        let _ = fs::set_permissions(&staging, meta.permissions());
    }

    fs::rename(&staging, &target)
        .with_context(|| format!("Failed to write file: {}", target.display()))?;
    guard.disarm();

    // Best-effort fsync of the directory so the rename — not just the staged
    // bytes — has a chance to be durable across a power loss. The edit has
    // already landed via the atomic rename, so a failure here doesn't fail
    // the command.
    if let Ok(dir) = fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// RAII cleanup for the staging file: removes it on drop unless the rename
/// landed, so a failed write never leaves an orphan dotfile in the project.
struct StagingFile {
    path: PathBuf,
    armed: bool,
}

impl StagingFile {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StagingFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

// ---------------------------------------------------------------------------
// Store refresh
// ---------------------------------------------------------------------------

/// Best-effort post-write catch-up for everything that answers about
/// files, through the same `StoreService`/`LspService` the rest of the
/// command layer uses — so it honors daemon/direct mode instead of
/// reaching for the daemon directly. The store re-extracts each file's
/// rows from the bytes just written (or drops them, if the file no longer
/// exists), so a search immediately after an edit sees the new content —
/// a store that was never built stays untouched. The LSP layer is told
/// the same files changed, so cached workspace answers expire and a live
/// server's overlay is synced and saved (fresh diagnostics on a warm
/// daemon). Failures are logged, never fail the edit that already landed.
pub(crate) async fn refresh_store_files(app: &App, files: &[PathBuf]) {
    if let Err(e) = app.store.refresh_files(files).await {
        tracing::warn!("Store refresh failed after edit: {e}");
    }
    app.lsp.note_files_edited(files).await;
}

// ---------------------------------------------------------------------------
// Workspace edit applier (LSP-computed edits; used by rename and actions)
// ---------------------------------------------------------------------------

use crate::models::lsp::{FileChangeWithEdits, TextEdit};

/// Apply workspace edits from LSP to actual files.
/// Used by rename and actions apply commands.
///
/// Returns the number of files modified and details of changes.
pub fn apply_workspace_edits(
    changes: &[FileChangeWithEdits],
    dry_run: bool,
    root: &Path,
) -> Result<Vec<AppliedFileChange>> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    // Coalesce edits by the file they ultimately resolve to. A workspace
    // edit's ranges are all expressed against the original document, so two
    // edit groups naming the same file (distinct URI spellings, or a server
    // that splits one document across entries) must apply together against
    // one read — staging them independently would let the later write clobber
    // the earlier one's changes.
    let grouped = coalesce_changes_by_file(changes);

    // Two phases: validate and stage every file in memory first, write
    // only after the whole edit checked out. A stale range in file N
    // must not leave files 1..N-1 already rewritten while the command
    // reports failure.
    let mut staged = Vec::with_capacity(grouped.len());

    for (file, edits) in grouped {
        // The paths come from the language server, not the user — they
        // still don't get to write outside the project.
        let canonical = file
            .canonicalize()
            .with_context(|| format!("Cannot resolve path: {}", file.display()))?;
        if !canonical.starts_with(&root) {
            anyhow::bail!(crate::cli::CliInputError::PathOutsideProject(canonical));
        }

        validate_file_for_edit(&file, !dry_run)?;

        let content = fs::read_to_string(&file)
            .with_context(|| format!("Failed to read file: {}", file.display()))?;

        let new_content = apply_text_edits(&content, &edits)
            .with_context(|| format!("Invalid edit for {}", file.display()))?;

        staged.push((file, new_content, edits.len()));
    }

    let mut results = Vec::with_capacity(staged.len());
    for (file, new_content, edit_count) in staged {
        if !dry_run {
            atomic_write(&file, &new_content)?;
        }
        results.push(AppliedFileChange {
            file,
            edit_count,
            applied: !dry_run,
        });
    }

    Ok(results)
}

/// Group workspace-edit entries by the file they resolve to, concatenating
/// the edits of any that target the same file while preserving first-seen
/// order. The key is the canonicalized path so distinct spellings of one
/// existing file coalesce; a path that can't be canonicalized (it doesn't
/// exist yet) falls back to its literal spelling, which still groups an
/// entry with itself.
fn coalesce_changes_by_file(changes: &[FileChangeWithEdits]) -> Vec<(PathBuf, Vec<TextEdit>)> {
    let mut order: Vec<PathBuf> = Vec::new();
    let mut by_key: std::collections::HashMap<PathBuf, (PathBuf, Vec<TextEdit>)> =
        std::collections::HashMap::new();

    for change in changes {
        let key = change
            .file
            .canonicalize()
            .unwrap_or_else(|_| change.file.clone());
        match by_key.get_mut(&key) {
            Some((_, edits)) => edits.extend(change.edits.iter().cloned()),
            None => {
                order.push(key.clone());
                by_key.insert(key, (change.file.clone(), change.edits.clone()));
            }
        }
    }

    order
        .into_iter()
        .filter_map(|key| by_key.remove(&key))
        .collect()
}

/// Result of applying edits to a single file
#[derive(Debug, Clone)]
pub struct AppliedFileChange {
    pub file: PathBuf,
    pub edit_count: usize,
    pub applied: bool,
}

/// Apply text edits to `content`. Every range is resolved once against the
/// original document — a stale (past-EOF) or inverted range aborts the whole
/// application rather than letting a partial result report success.
///
/// The result is rebuilt left-to-right from the original content: edits are
/// taken in ascending start order and each contributes `content[prev..start]`
/// followed by its replacement text. Because nothing is ever applied against
/// bytes an earlier edit already moved, the original offsets stay valid for
/// every edit — including a zero-width insert that shares a boundary with an
/// adjacent replace. An edit that starts before the previous one ended is a
/// genuine overlap (edits from one revision don't overlap) and is refused
/// rather than silently misapplied. Several inserts at one position keep the
/// order they were given.
pub(crate) fn apply_text_edits(content: &str, edits: &[TextEdit]) -> Result<String> {
    if edits.is_empty() {
        return Ok(content.to_string());
    }

    // Resolve to original-document byte offsets up front. `order` keeps
    // same-position inserts in the order they were given.
    let mut resolved: Vec<ResolvedEdit> = Vec::with_capacity(edits.len());
    for (order, edit) in edits.iter().enumerate() {
        let start = line_char_to_byte_offset(
            content,
            edit.range.start.line as usize,
            edit.range.start.character as usize,
        )?;
        let end = line_char_to_byte_offset(
            content,
            edit.range.end.line as usize,
            edit.range.end.character as usize,
        )?;
        if start > end {
            anyhow::bail!(stale_revision(format!(
                "LSP edit range {:?} is inverted; the edit was computed \
                 against a different revision — retry",
                edit.range,
            )));
        }
        resolved.push(ResolvedEdit {
            start,
            end,
            order,
            text: &edit.new_text,
        });
    }

    // Ascending by start, then end (a zero-width insert sorts before a
    // same-start replace), then given order (same-position inserts keep
    // their order). One ordering drives both overlap checking and assembly.
    resolved.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then(a.end.cmp(&b.end))
            .then(a.order.cmp(&b.order))
    });

    let mut result = String::with_capacity(content.len());
    let mut pos = 0usize;
    for e in &resolved {
        if e.start < pos {
            anyhow::bail!(stale_revision(format!(
                "overlapping LSP edits near byte {}; the edit set was computed \
                 against a different revision — retry",
                e.start,
            )));
        }
        result.push_str(&content[pos..e.start]);
        result.push_str(e.text);
        pos = e.end;
    }
    result.push_str(&content[pos..]);
    Ok(result)
}

/// One LSP edit resolved to original-document byte offsets. `order` is the
/// index in the given edit list, used only to keep same-position inserts in
/// the order they were given.
struct ResolvedEdit<'a> {
    start: usize,
    end: usize,
    order: usize,
    text: &'a str,
}

/// Byte offset for an LSP (0-indexed) line/character position.
///
/// A character past the end of its line clamps to the line end — that is
/// the LSP-specified meaning of such positions, not a guess. A line past
/// the end of the document has no specified meaning: it means the server
/// computed the edit against a newer revision than the one on disk, and
/// silently clamping it to EOF would apply the edit somewhere the server
/// never asked for.
fn line_char_to_byte_offset(content: &str, line: usize, character: usize) -> Result<usize> {
    let mut byte_offset = 0;
    let mut lines_seen = 0;

    for line_content in content.lines() {
        if lines_seen == line {
            return Ok(byte_offset + char_to_byte_index(line_content, character));
        }
        byte_offset += line_content.len();
        // Account for actual line ending bytes (\r\n = 2, \n = 1)
        let remaining = &content.as_bytes()[byte_offset..];
        if remaining.starts_with(b"\r\n") {
            byte_offset += 2;
        } else if remaining.starts_with(b"\n") {
            byte_offset += 1;
        }
        lines_seen += 1;
    }

    // Exactly one line past the last content line is the canonical LSP
    // end-of-document position (an append point), whether or not the file
    // ends in a newline. A line further past that is a different revision —
    // clamping it would land the edit somewhere the server never asked for.
    if line == lines_seen {
        return Ok(content.len());
    }

    anyhow::bail!(stale_revision(format!(
        "LSP edit range line {} exceeds the document's {} lines; the edit \
         was computed against a different revision — retry",
        line + 1,
        lines_seen,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::utils::{
        SymbolResolution, column_addressed_symbol, line_addressed_symbol, symbols_declared_on_line,
    };
    use crate::models::symbol::{Location, SymbolKind};

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn sample_symbol(start: u32, end: u32) -> Symbol {
        Symbol::new(
            "process".to_string(),
            SymbolKind::Function,
            Location::full(PathBuf::from("/tmp/foo.rs"), start, 1, start, 1, end, 1),
        )
    }

    /// A method inside an impl/class, addressed line-only: the symbol
    /// DECLARED on the line wins over the enclosing block whose range
    /// also contains it.
    #[test]
    fn line_addressing_prefers_the_symbol_declared_on_the_line() {
        let symbols = impl_with_method();
        match line_addressed_symbol(&symbols, 26) {
            SymbolResolution::Match(symbol) => assert_eq!(symbol.name, "new"),
            _ => panic!("expected the method declared on line 26"),
        }
    }

    /// A body line (no declaration) still resolves to the innermost
    /// enclosing symbol.
    #[test]
    fn line_addressing_falls_back_to_the_enclosing_symbol_inside_a_body() {
        let symbols = impl_with_method();
        match line_addressed_symbol(&symbols, 27) {
            SymbolResolution::Match(symbol) => assert_eq!(symbol.name, "new"),
            _ => panic!("expected the enclosing method for a body line"),
        }
        match line_addressed_symbol(&symbols, 31) {
            SymbolResolution::Match(symbol) => assert_eq!(symbol.name, "Rect"),
            _ => panic!("expected the enclosing impl for a line between methods"),
        }
    }

    #[test]
    fn line_addressing_reports_multiple_declarations_as_ambiguous() {
        let symbols = vec![
            Symbol::new(
                "A".to_string(),
                SymbolKind::Constant,
                Location::full(PathBuf::from("/tmp/foo.rs"), 5, 7, 5, 1, 5, 18),
            ),
            Symbol::new(
                "B".to_string(),
                SymbolKind::Constant,
                Location::full(PathBuf::from("/tmp/foo.rs"), 5, 26, 5, 20, 5, 37),
            ),
        ];
        match line_addressed_symbol(&symbols, 5) {
            SymbolResolution::Ambiguous(declared) => assert_eq!(declared.len(), 2),
            _ => panic!("two declarations on one line must stay ambiguous"),
        }
    }

    /// `file:line:col` semantics are untouched by the line-only rule: an
    /// exact column resolves by range, so a column inside the impl but
    /// before the method name still addresses the impl.
    #[test]
    fn column_addressing_keeps_position_precise_range_matching() {
        let symbols = impl_with_method();
        match column_addressed_symbol(&symbols, 26, 12) {
            SymbolResolution::Match(symbol) => assert_eq!(symbol.name, "new"),
            _ => panic!("the method name column must address the method"),
        }
        match column_addressed_symbol(&symbols, 26, 1) {
            SymbolResolution::Match(symbol) => assert_eq!(symbol.name, "Rect"),
            _ => panic!("column 1 falls in the impl's interior range"),
        }
    }

    /// `impl Rect` spanning lines 24..=32 with `pub fn new` declared on
    /// line 26 (name at column 12), body through line 29.
    fn impl_with_method() -> Vec<Symbol> {
        let method = Symbol::new(
            "new".to_string(),
            SymbolKind::Function,
            Location::full(PathBuf::from("/tmp/foo.rs"), 26, 12, 26, 5, 29, 6),
        );
        vec![
            Symbol::new(
                "Rect".to_string(),
                SymbolKind::Struct,
                Location::full(PathBuf::from("/tmp/foo.rs"), 24, 6, 24, 1, 32, 2),
            )
            .with_children(vec![method]),
        ]
    }

    #[test]
    fn replace_body_splice_replaces_inclusive_span() {
        let original = lines(&["line1", "fn process() {", "    body", "}", "line5"]);
        let splice = LineSplice {
            at: 1,
            removed: 3,
            new_lines: lines(&["fn process() { new() }"]),
        };
        assert_eq!(
            splice.apply(&original),
            lines(&["line1", "fn process() { new() }", "line5"])
        );
    }

    #[test]
    fn insert_splices_do_not_remove_lines() {
        let original = lines(&["line1", "fn process() {", "}", "line4"]);
        let before = LineSplice {
            at: 1,
            removed: 0,
            new_lines: lines(&["// hello"]),
        };
        assert_eq!(
            before.apply(&original),
            lines(&["line1", "// hello", "fn process() {", "}", "line4"])
        );
        let after = LineSplice {
            at: 3,
            removed: 0,
            new_lines: lines(&["// trailer"]),
        };
        assert_eq!(
            after.apply(&original),
            lines(&["line1", "fn process() {", "}", "// trailer", "line4"])
        );
    }

    /// The hunk is derived from the splice, so an insert shows exactly
    /// the inserted lines — the positional-diff failure mode (every line
    /// after the insert marked changed) cannot occur.
    #[test]
    fn insert_hunk_contains_only_the_inserted_lines() {
        let original = lines(&["a", "b", "c", "d", "e"]);
        let splice = LineSplice {
            at: 2,
            removed: 0,
            new_lines: lines(&["x"]),
        };
        assert_eq!(splice.unified_hunk(&original), "@@ -3,0 +3,1 @@\n+x\n");
    }

    #[test]
    fn replace_hunk_lists_removed_then_added() {
        let original = lines(&["a", "old1", "old2", "d"]);
        let splice = LineSplice {
            at: 1,
            removed: 2,
            new_lines: lines(&["new"]),
        };
        assert_eq!(
            splice.unified_hunk(&original),
            "@@ -2,2 +2,1 @@\n-old1\n-old2\n+new\n"
        );
    }

    #[test]
    fn char_splice_is_character_indexed_not_byte_indexed() {
        // '한' and '글' are 3 bytes each; column 3 must mean the third
        // *character*.
        let original = lines(&["한글ab"]);
        let splice = char_splice(&original, 1, 3, 1, Some(4), "X", None).unwrap();
        assert_eq!(splice.new_lines, lines(&["한글Xb"]));
    }

    #[test]
    fn char_splice_without_end_col_replaces_through_eol() {
        let original = lines(&["keep: drop", "next"]);
        let splice = char_splice(&original, 1, 7, 1, None, "kept", None).unwrap();
        assert_eq!(splice.new_lines, lines(&["keep: kept"]));
        assert_eq!(splice.removed, 1);
    }

    #[test]
    fn char_splice_spanning_lines_merges_prefix_and_suffix() {
        let original = lines(&["start AAA", "BBB end"]);
        let splice = char_splice(&original, 1, 7, 2, Some(4), "X", None).unwrap();
        assert_eq!(splice.new_lines, lines(&["start X end"]));
        assert_eq!(splice.removed, 2);
    }

    /// Positions are validated, never clamped — a column past EOL is the
    /// caller addressing content that isn't there.
    #[test]
    fn char_splice_rejects_out_of_range_columns() {
        let original = lines(&["short"]);
        // chars+1 (= 6) is the valid zero-width EOL position; 7 is not.
        assert!(char_splice(&original, 1, 6, 1, None, "x", None).is_ok());
        let err = char_splice(&original, 1, 7, 1, None, "x", None).unwrap_err();
        assert!(err.to_string().contains("exceeds line 1 length"));
        let err = char_splice(&original, 1, 1, 1, Some(7), "x", None).unwrap_err();
        assert!(err.to_string().contains("exceeds line 1 length"));
    }

    /// An inverted same-line range would slice overlapping prefix and
    /// suffix and duplicate the text between them.
    #[test]
    fn char_splice_rejects_inverted_same_line_range() {
        let original = lines(&["abcdef"]);
        let err = char_splice(&original, 1, 4, 1, Some(2), "x", None).unwrap_err();
        assert!(err.to_string().contains("precedes start column"));
        // Equal start/end is a valid zero-width insert.
        assert!(char_splice(&original, 1, 4, 1, Some(4), "x", None).is_ok());
    }

    /// CRLF replacement text must not leak `\r` into the line array —
    /// `render` re-joins with the file's own ending.
    #[test]
    fn char_splice_normalizes_crlf_replacement_text() {
        let original = lines(&["old"]);
        let splice = char_splice(&original, 1, 1, 1, None, "a\r\nb", None).unwrap();
        assert_eq!(splice.new_lines, lines(&["a", "b"]));
    }

    /// An empty file has exactly one addressable position: 1:1.
    #[test]
    fn char_splice_accepts_insert_into_empty_file_at_origin() {
        let empty: Vec<String> = vec![];
        let splice = char_splice(&empty, 1, 1, 1, None, "hello\nworld", None).unwrap();
        assert_eq!(splice.at, 0);
        assert_eq!(splice.removed, 0);
        assert_eq!(splice.new_lines, lines(&["hello", "world"]));
        assert!(char_splice(&empty, 2, 1, 2, None, "x", None).is_err());
    }

    /// `--expect` matching the live text lets the splice proceed; a single-
    /// line range compares the spanned characters exactly.
    #[test]
    fn char_splice_expect_matches_live_text() {
        let original = lines(&["let x = 1;"]);
        // cols 5..6 (1-indexed, end exclusive) span "x".
        let splice = char_splice(&original, 1, 5, 1, Some(6), "y", Some("x")).unwrap();
        assert_eq!(splice.new_lines, lines(&["let y = 1;"]));
    }

    /// A mismatched `--expect` aborts as a `Conflict` — the branchable
    /// "re-read and retry" signal, never an internal error.
    #[test]
    fn char_splice_expect_mismatch_is_a_conflict() {
        use crate::cli::{ErrorCode, OutputError};
        let original = lines(&["let x = 1;"]);
        let err = char_splice(&original, 1, 5, 1, Some(6), "y", Some("z")).unwrap_err();
        let out: OutputError = err.into();
        assert!(matches!(out.code, ErrorCode::Conflict));
        assert!(out.message.contains("does not match --expect"));
    }

    /// `--expect` spanning lines reconstructs the region as `\n`-joined and
    /// treats a CRLF expectation as equal to the LF-stored text.
    #[test]
    fn char_splice_expect_tolerates_crlf_across_lines() {
        let original = lines(&["a", "b"]);
        // 1:1..2:2 spans "a\nb".
        assert!(char_splice(&original, 1, 1, 2, Some(2), "x", Some("a\r\nb")).is_ok());
        // Indentation differences are NOT tolerated.
        let indented = lines(&["  a", "b"]);
        assert!(char_splice(&indented, 1, 1, 2, Some(2), "x", Some("a\nb")).is_err());
    }

    /// On an empty file the only expectation the origin can satisfy is the
    /// empty string; anything else is a conflict.
    #[test]
    fn char_splice_expect_on_empty_file() {
        let empty: Vec<String> = vec![];
        assert!(char_splice(&empty, 1, 1, 1, None, "x", Some("")).is_ok());
        assert!(char_splice(&empty, 1, 1, 1, None, "x", Some("y")).is_err());
    }

    /// Geometry drift with `--expect` (the file shrank so the range no longer
    /// fits) is a `Conflict` to re-read and retry — NOT an `InvalidArgument`,
    /// which an agent would treat as a bad request and not retry. Without
    /// `--expect`, the same out-of-range column stays `InvalidArgument`.
    #[test]
    fn char_splice_expect_out_of_range_is_a_conflict() {
        use crate::cli::{ErrorCode, OutputError};
        let original = lines(&["short"]); // 5 chars; col 20 no longer exists
        let err = char_splice(&original, 1, 1, 1, Some(20), "x", Some("anything")).unwrap_err();
        let out: OutputError = err.into();
        assert!(
            matches!(out.code, ErrorCode::Conflict),
            "out-of-range with --expect must be a Conflict"
        );

        // The same geometry without --expect is a plain invalid-argument.
        let err = char_splice(&original, 1, 1, 1, Some(20), "x", None).unwrap_err();
        let out: OutputError = err.into();
        assert!(matches!(out.code, ErrorCode::InvalidArgument));
    }

    /// The only state that satisfies the no-references precondition: the
    /// check ran, found zero dangling references, with no degradation.
    #[test]
    fn no_references_guard_passes_on_verified_zero() {
        let check = ReferenceCheck::Checked(Section::new(vec![]));
        assert!(
            ensure_no_dangling_references("process", "symora refs src/foo.rs:2:1", &check).is_ok()
        );
    }

    /// Dangling references refuse with the pre-cap total in the message
    /// and the ready-to-run refs command in the hint.
    #[test]
    fn no_references_guard_refuses_dangling_references() {
        use crate::cli::{ErrorCode, OutputError};
        let check = ReferenceCheck::Checked(Section::with_total(Vec::new(), 2));
        let err = ensure_no_dangling_references("process", "symora refs src/foo.rs:2:1", &check)
            .unwrap_err();
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
        let err = ensure_no_dangling_references("process", "symora refs src/foo.rs:2:1", &check)
            .unwrap_err();
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
        let err = ensure_no_dangling_references("process", "symora refs src/foo.rs:2:1", &check)
            .unwrap_err();
        let out: OutputError = err.into();
        assert!(matches!(out.code, ErrorCode::PreconditionFailed));
        assert!(out.message.contains("references_status: unavailable"));
        assert!(out.hint.unwrap().contains("doctor"));
    }

    /// A zero computed under degraded indexing is a lower bound, not a
    /// verified zero — it refuses, naming the degradation.
    #[test]
    fn no_references_guard_refuses_degraded_zero_as_unverified() {
        use crate::cli::{ErrorCode, OutputError};
        use crate::models::lsp::IndexingDegradation;
        let check = ReferenceCheck::Checked(
            Section::new(vec![]).with_indexing(Some(IndexingDegradation::TimedOut)),
        );
        let err = ensure_no_dangling_references("process", "symora refs src/foo.rs:2:1", &check)
            .unwrap_err();
        let out: OutputError = err.into();
        assert!(matches!(out.code, ErrorCode::PreconditionFailed));
        assert!(out.message.contains("lower bound"));
        assert!(out.message.contains("timed_out"));
    }

    /// A non-zero count under degradation is stated as "at least" so the
    /// agent never treats it as exhaustive when fixing call sites.
    #[test]
    fn no_references_guard_marks_degraded_count_as_lower_bound() {
        use crate::cli::{ErrorCode, OutputError};
        use crate::models::lsp::IndexingDegradation;
        let check = ReferenceCheck::Checked(
            Section::with_total(Vec::new(), 3).with_indexing(Some(IndexingDegradation::TimedOut)),
        );
        let err = ensure_no_dangling_references("process", "symora refs src/foo.rs:2:1", &check)
            .unwrap_err();
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
        let err = ensure_exclusive_line_ownership(&content, &shared, &span).unwrap_err();
        assert!(err.to_string().contains("shares its first line"));

        // Sole occupant of its lines passes (ends at exclusive column 24,
        // one past the 23-char line).
        let solo = lines(&["fn process() { body() }"]);
        let alone = Symbol::new(
            "process".to_string(),
            SymbolKind::Function,
            Location::full(PathBuf::from("/tmp/a.rs"), 1, 4, 1, 1, 1, 24),
        );
        assert!(ensure_exclusive_line_ownership(&solo, &alone, &span).is_ok());
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
        let err = ensure_exclusive_line_ownership(&content, &sym, &span).unwrap_err();
        assert!(err.to_string().contains("shares its last line"));
    }

    fn path_candidate(line: u32) -> Symbol {
        Symbol::new(
            "bar".to_string(),
            SymbolKind::Method,
            Location::point(PathBuf::from("/tmp/a.rs"), line, 5),
        )
    }

    /// A pattern matching several symbols is an under-specified target the
    /// agent fixes by re-addressing — same code as the multi-symbol-line
    /// case, with the candidates' lines as the honest disambiguator.
    #[test]
    fn ambiguous_symbol_path_is_invalid_argument_with_candidates() {
        use crate::cli::{ErrorCode, OutputError};
        let first = path_candidate(10);
        let second = path_candidate(40);
        let out: OutputError = ambiguous_symbol_path("Foo/bar", &[&first, &second], "a.rs").into();
        assert!(matches!(out.code, ErrorCode::InvalidArgument));
        assert!(out.message.contains("matches 2 symbols"));
        let hint = out.hint.unwrap();
        assert!(hint.contains("line 10") && hint.contains("line 40"));
        assert!(hint.contains("file:line"));
    }

    #[test]
    fn ambiguous_symbol_path_hint_caps_candidates() {
        use crate::cli::OutputError;
        let symbols: Vec<Symbol> = (1..=7).map(|i| path_candidate(i * 10)).collect();
        let candidates: Vec<&Symbol> = symbols.iter().collect();
        let out: OutputError = ambiguous_symbol_path("Foo/bar", &candidates, "a.rs").into();
        assert!(out.message.contains("matches 7 symbols"));
        let hint = out.hint.unwrap();
        assert!(hint.contains("+2 more"));
        assert!(hint.contains("line 50") && !hint.contains("line 60"));
    }

    #[test]
    fn symbol_not_found_is_structured() {
        use crate::cli::{ErrorCode, OutputError};
        let out: OutputError = symbol_not_found("Foo/bar", "src/a.rs").into();
        assert!(matches!(out.code, ErrorCode::NotFound));
        assert!(out.message.contains("Foo/bar"));
        assert!(out.hint.unwrap().contains("symora symbols"));
    }

    /// Two structs in a file, each with an `area` method. Build the symbol
    /// set both the unique and the ambiguous `--symbol` dispatch resolve
    /// against (paths pre-computed, as the live caller does).
    fn two_structs_with_area() -> Vec<Symbol> {
        let make = |name: &str, line: u32| {
            Symbol::new(
                name.to_string(),
                SymbolKind::Struct,
                Location::full(PathBuf::from("/tmp/foo.rs"), line, 8, line, 1, line + 3, 2),
            )
            .with_children(vec![Symbol::new(
                "area".to_string(),
                SymbolKind::Function,
                Location::full(
                    PathBuf::from("/tmp/foo.rs"),
                    line + 1,
                    8,
                    line + 1,
                    5,
                    line + 2,
                    6,
                ),
            )])
        };
        let mut symbols = vec![make("Rect", 1), make("Circle", 10)];
        Symbol::compute_paths_for_all(&mut symbols);
        symbols
    }

    /// The destructive `--symbol` dispatch resolves a pattern that names
    /// exactly one symbol — including the bare last-component and `*/method`
    /// wildcard forms the flexible matcher accepts.
    #[test]
    fn unique_symbol_by_path_resolves_a_single_match() {
        let mut symbols = impl_with_method();
        Symbol::compute_paths_for_all(&mut symbols);
        assert_eq!(
            unique_symbol_by_path(&symbols, "new", "foo.rs")
                .unwrap()
                .name,
            "new"
        );
        assert_eq!(
            unique_symbol_by_path(&symbols, "*/new", "foo.rs")
                .unwrap()
                .name,
            "new"
        );
        assert_eq!(
            unique_symbol_by_path(&symbols, "Rect/new", "foo.rs")
                .unwrap()
                .name,
            "new"
        );
    }

    /// A bare name hitting the same method on two distinct parents is an
    /// ambiguity the destructive edit refuses — never a silent arbitrary
    /// pick.
    #[test]
    fn unique_symbol_by_path_refuses_an_ambiguous_pattern() {
        use crate::cli::{ErrorCode, OutputError};
        let symbols = two_structs_with_area();
        let out: OutputError = unique_symbol_by_path(&symbols, "area", "foo.rs")
            .unwrap_err()
            .into();
        assert!(matches!(out.code, ErrorCode::InvalidArgument));
        assert!(out.message.contains("matches 2 symbols"));
    }

    /// A pattern that matches nothing is a structured not-found.
    #[test]
    fn unique_symbol_by_path_reports_a_miss_as_not_found() {
        use crate::cli::{ErrorCode, OutputError};
        let symbols = two_structs_with_area();
        let out: OutputError = unique_symbol_by_path(&symbols, "nonexistent", "foo.rs")
            .unwrap_err()
            .into();
        assert!(matches!(out.code, ErrorCode::NotFound));
    }

    #[test]
    fn conflicting_addressing_is_invalid_argument() {
        use crate::cli::{ErrorCode, OutputError};
        let out: OutputError = conflicting_addressing("src/a.rs:10:1").into();
        assert!(matches!(out.code, ErrorCode::InvalidArgument));
        assert!(out.hint.unwrap().contains("--symbol"));
    }

    /// A symbol on a multi-symbol line resolves only with a matching
    /// column; nothing is guessed on a precise miss.
    #[test]
    fn declared_on_line_finds_same_line_siblings() {
        let a = Symbol::new(
            "alpha".to_string(),
            SymbolKind::Function,
            Location::full(PathBuf::from("/tmp/a.rs"), 1, 4, 1, 1, 1, 20),
        );
        let b = Symbol::new(
            "beta".to_string(),
            SymbolKind::Function,
            Location::full(PathBuf::from("/tmp/a.rs"), 1, 28, 1, 25, 1, 44),
        );
        let symbols = vec![a, b];
        let found = symbols_declared_on_line(&symbols, 1);
        assert_eq!(found.len(), 2);
        assert!(symbols_declared_on_line(&symbols, 2).is_empty());
    }

    /// Workspace-edit ranges are validated, never clamped: the canonical
    /// end-of-document append position works, anything past it is stale.
    #[test]
    fn lsp_offsets_validate_instead_of_clamping() {
        // Char past line end clamps per the LSP spec.
        assert_eq!(line_char_to_byte_offset("ab\ncd\n", 0, 99).unwrap(), 2);
        // Canonical end-of-document position (after the final newline).
        assert_eq!(line_char_to_byte_offset("ab\ncd\n", 2, 0).unwrap(), 6);
        assert_eq!(line_char_to_byte_offset("", 0, 0).unwrap(), 0);
        // One line past the last content line is the EOF append point even
        // when the file has no trailing newline.
        assert_eq!(line_char_to_byte_offset("ab\ncd", 2, 0).unwrap(), 5);
        // Two or more lines past EOF is a stale revision, not an append.
        assert!(line_char_to_byte_offset("ab\ncd\n", 3, 0).is_err());
        assert!(line_char_to_byte_offset("ab\ncd", 3, 0).is_err());
    }

    #[test]
    fn stale_text_edit_aborts_instead_of_partially_applying() {
        use crate::models::lsp::{Position, Range as LspRange, TextEdit};
        let good = TextEdit {
            range: LspRange::new(Position::new(0, 0), Position::new(0, 2)),
            new_text: "xy".to_string(),
        };
        let stale = TextEdit {
            range: LspRange::new(Position::new(9, 0), Position::new(9, 1)),
            new_text: "z".to_string(),
        };
        assert_eq!(
            apply_text_edits("ab\ncd\n", std::slice::from_ref(&good)).unwrap(),
            "xy\ncd\n"
        );
        let err = apply_text_edits("ab\ncd\n", &[good, stale]).unwrap_err();
        assert!(err.to_string().contains("different revision"));
    }

    #[test]
    fn overlapping_edits_are_rejected_not_misapplied() {
        use crate::models::lsp::{Position, Range as LspRange, TextEdit};
        // Two edits whose ranges intersect in the original document can't both
        // be honored — applying one corrupts the other's coordinates.
        let a = TextEdit {
            range: LspRange::new(Position::new(0, 0), Position::new(0, 4)),
            new_text: "X".to_string(),
        };
        let b = TextEdit {
            range: LspRange::new(Position::new(0, 2), Position::new(0, 6)),
            new_text: "Y".to_string(),
        };
        let err = apply_text_edits("abcdef\n", &[a, b]).unwrap_err();
        assert!(err.to_string().contains("overlapping"));
        // The stale-revision guards surface as a branchable `Conflict`, not a
        // generic internal error — pinned here so the migration can't regress.
        let out: crate::cli::OutputError = err.into();
        assert!(matches!(out.code, crate::cli::ErrorCode::Conflict));
    }

    #[test]
    fn overlap_hidden_behind_an_interleaved_insert_is_still_rejected() {
        use crate::models::lsp::{Position, Range as LspRange, TextEdit};
        // A zero-width insert that sorts between two overlapping non-empty
        // ranges must not mask the overlap: validation scans all ranges, not
        // just adjacent ones.
        let wide = TextEdit {
            range: LspRange::new(Position::new(0, 0), Position::new(0, 10)),
            new_text: "R".to_string(),
        };
        let insert = TextEdit {
            range: LspRange::new(Position::new(0, 0), Position::new(0, 0)),
            new_text: "I".to_string(),
        };
        let inner = TextEdit {
            range: LspRange::new(Position::new(0, 5), Position::new(0, 6)),
            new_text: "X".to_string(),
        };
        let err = apply_text_edits("0123456789abc\n", &[wide, insert, inner]).unwrap_err();
        assert!(err.to_string().contains("overlapping"));
    }

    #[test]
    fn insert_sharing_a_start_with_a_replace_keeps_both() {
        use crate::models::lsp::{Position, Range as LspRange, TextEdit};
        // A zero-width insert at the same position as a replace's start must
        // land before the replacement, not be clobbered by it.
        let replace = TextEdit {
            range: LspRange::new(Position::new(0, 0), Position::new(0, 2)),
            new_text: "X".to_string(),
        };
        let insert = TextEdit {
            range: LspRange::new(Position::new(0, 0), Position::new(0, 0)),
            new_text: "I".to_string(),
        };
        assert_eq!(
            apply_text_edits("abcd\n", &[replace, insert]).unwrap(),
            "IXcd\n"
        );
    }

    #[test]
    fn edit_at_eof_sentinel_applies_without_a_trailing_newline() {
        use crate::models::lsp::{Position, Range as LspRange, TextEdit};
        // A formatter appending a trailing newline addresses the position one
        // line past the last content line; that must apply, not error, on a
        // file that doesn't already end in a newline.
        let append = TextEdit {
            range: LspRange::new(Position::new(1, 1), Position::new(2, 0)),
            new_text: "\n".to_string(),
        };
        assert_eq!(apply_text_edits("a\nb", &[append]).unwrap(), "a\nb\n");
    }

    #[test]
    fn adjacent_edits_apply_without_shifting_each_other() {
        use crate::models::lsp::{Position, Range as LspRange, TextEdit};
        let first = TextEdit {
            range: LspRange::new(Position::new(0, 0), Position::new(0, 2)),
            new_text: "XX".to_string(),
        };
        let second = TextEdit {
            range: LspRange::new(Position::new(0, 2), Position::new(0, 4)),
            new_text: "YY".to_string(),
        };
        // Given in document order, not sorted — the applier must order them.
        assert_eq!(
            apply_text_edits("abcd\n", &[first, second]).unwrap(),
            "XXYY\n"
        );
    }

    #[test]
    fn same_position_inserts_keep_given_order() {
        use crate::models::lsp::{Position, Range as LspRange, TextEdit};
        let insert = |text: &str| TextEdit {
            range: LspRange::new(Position::new(0, 0), Position::new(0, 0)),
            new_text: text.to_string(),
        };
        // Two zero-width inserts at the same point must land in the order
        // they were given (A before B), not reversed.
        assert_eq!(
            apply_text_edits("z\n", &[insert("A"), insert("B")]).unwrap(),
            "ABz\n"
        );
    }

    #[test]
    fn symbol_line_span_rejects_stale_lsp_data() {
        let sym = sample_symbol(1, 100);
        let err = symbol_line_span(&sym, 5).unwrap_err();
        assert!(err.to_string().contains("exceeds file length"));
    }

    #[test]
    fn symbol_line_span_rejects_a_stale_anchor_into_an_emptied_file() {
        // The file was emptied since the symbol resolved (0 lines): a line-1
        // anchor must fail closed as stale, not slip past `total_lines.max(1)`
        // into the splice where it would surface as a misleading range error.
        let sym = sample_symbol(1, 1);
        let err = symbol_line_span(&sym, 0).unwrap_err();
        assert!(err.to_string().contains("exceeds file length"));
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

    #[test]
    fn coalesce_merges_edits_for_the_same_file_in_order() {
        use crate::models::lsp::{Position, Range as LspRange, TextEdit};
        let edit = |line: u32, text: &str| TextEdit {
            range: LspRange::new(Position::new(line, 0), Position::new(line, 0)),
            new_text: text.to_string(),
        };
        // Two entries naming the same file (as a workspace edit's
        // documentChanges may) must collapse into one group carrying both
        // edits, so a later write can't clobber the earlier one's changes.
        let changes = vec![
            FileChangeWithEdits {
                file: PathBuf::from("a.rs"),
                edits: vec![edit(0, "x")],
            },
            FileChangeWithEdits {
                file: PathBuf::from("b.rs"),
                edits: vec![edit(0, "y")],
            },
            FileChangeWithEdits {
                file: PathBuf::from("a.rs"),
                edits: vec![edit(1, "z")],
            },
        ];
        let grouped = coalesce_changes_by_file(&changes);
        assert_eq!(grouped.len(), 2, "a.rs must appear once");
        assert_eq!(grouped[0].0, PathBuf::from("a.rs"));
        assert_eq!(grouped[0].1.len(), 2, "both a.rs edits retained");
        assert_eq!(grouped[1].0, PathBuf::from("b.rs"));
        assert_eq!(grouped[1].1.len(), 1);
    }

    #[test]
    fn symbol_line_span_includes_full_declaration() {
        let sym = sample_symbol(2, 4);
        let span = symbol_line_span(&sym, 10).unwrap();
        assert_eq!((span.start, span.end), (2, 4));
    }

    #[test]
    fn atomic_write_round_trips_content_including_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("file.rs");
        fs::write(&target, "old").unwrap();

        let content = "fn main() {}\r\nfn aux() {}\r\n";
        atomic_write(&target, content).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), content);
    }

    #[test]
    fn atomic_write_leaves_no_staging_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("file.rs");
        fs::write(&target, "old").unwrap();

        atomic_write(&target, "new").unwrap();
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "staging file leaked: {leftovers:?}");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_target_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("script.sh");
        fs::write(&target, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

        atomic_write(&target, "#!/bin/sh\necho hi\n").unwrap();
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_edits_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.rs");
        let link = dir.path().join("link.rs");
        fs::write(&real, "old").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        atomic_write(&link, "new").unwrap();
        // The link must still be a symlink and the real file must hold the
        // new content — a rename onto the link itself would sever it.
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(&real).unwrap(), "new");
    }
}
