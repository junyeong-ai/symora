//! What a response says about its own shortfalls.
//!
//! A list answer is rarely the whole truth: an index built over paths nobody
//! could read, a walk turned away partway, a page that filled, a language
//! whose server never answered. Every one of those is a fact about the answer
//! plus a remedy that has to work against the tree as it is now, and the two
//! halves are one decision — which is why they are produced together here
//! rather than assembled per command.
//!
//! The vocabulary divides along two axes that must not be folded together:
//! [`LowerBound`] says the answer does not hold everything ITS SOURCES held,
//! and [`Uncovered`] says a LANGUAGE is missing from its domain. Currency —
//! whether a source is itself up to date — is neither, and is carried by
//! `stale`, `backend`, and `search index status`.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use super::{CoverageGap, Section};
use crate::cli::OutputContext;
use crate::error::{LspError, StoreError};
use crate::models::symbol::Language;
use crate::services::store::UnreadPath;

/// Why a language is missing from an answer, as a stable marker an agent
/// branches on — install a server, retry a timeout, narrow with `--lang`,
/// or build the index. The wire spelling is the published contract; the
/// enum keeps the set closed so every surface of a disclosure dispatches on
/// the same values instead of comparing strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoverageReason {
    /// Outside the search index's scope, on a route that answers from the
    /// index alone and so cannot ask anything else.
    NotIndexed,
    ServerNotInstalled,
    TimedOut,
    /// A capability gap — `is_unsupported` covers both the static table and
    /// a runtime JSON-RPC method-not-found — matching the central error
    /// classifier.
    Unsupported,
    Unavailable,
    /// Enough candidates came from other languages, so this one was never
    /// queried. Narrowing the query to it is the remedy.
    NotSearched,
}

impl CoverageReason {
    pub fn of(err: &LspError) -> Self {
        match err {
            LspError::ServerNotInstalled { .. } => Self::ServerNotInstalled,
            LspError::Timeout(_) => Self::TimedOut,
            LspError::UnsupportedLanguage(_) => Self::Unsupported,
            e if e.is_unsupported() => Self::Unsupported,
            _ => Self::Unavailable,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotIndexed => "not_indexed",
            Self::ServerNotInstalled => "server_not_installed",
            Self::TimedOut => "timed_out",
            Self::Unsupported => "unsupported",
            Self::Unavailable => "unavailable",
            Self::NotSearched => "not_searched",
        }
    }
}

/// A language an answer could not cover, paired with why. Typed so every
/// surface of the disclosure — the structured gap an agent branches on, the
/// prose hint, the follow-up command — is derived from this one value, and
/// none of them can name a language or a cause the others do not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Uncovered {
    pub language: Language,
    pub reason: CoverageReason,
}

impl From<Uncovered> for CoverageGap {
    fn from(uncovered: Uncovered) -> Self {
        Self {
            language: uncovered.language.lsp_id().to_string(),
            reason: uncovered.reason.as_str().to_string(),
        }
    }
}

/// Whether the route asked language servers for what the index could not
/// answer, and with what outcome.
pub enum LiveLookup<'a> {
    Ran {
        failures: &'a [(Language, LspError)],
        /// Languages the fan-out never reached — neither a failure nor an
        /// answer, and silent unless recorded here.
        skipped: &'a [Language],
    },
    /// The route answers from the index alone — a wildcard is matched
    /// against the index's own rows — so a language the index holds was
    /// searched, and one it does not hold is beyond this answer entirely.
    /// It carries the requested set because it is the only variant that reads
    /// one: a route that ran a fan-out names what it could not cover, while
    /// this one has to subtract from what was asked for.
    NotRun { requested: &'a [Language] },
}

/// The languages an answer cannot vouch for: requested, outside what the
/// answer speaks for, and not answered live either — because their lookup
/// failed, because the fan-out never reached them, or because the route
/// makes no live lookup at all. What an answer speaks for is the caller's
/// to state: a route that consults a language server is vouched only where
/// the index actually answered, while one that reads the index alone speaks
/// for everything the index holds. Sorted by language id for deterministic
/// output.
pub fn coverage_shortfall(answered_for: &[Language], live: LiveLookup<'_>) -> Vec<Uncovered> {
    let mut gaps: Vec<Uncovered> = match live {
        LiveLookup::Ran { failures, skipped } => failures
            .iter()
            .filter(|(language, _)| !answered_for.contains(language))
            .map(|(language, err)| Uncovered {
                language: *language,
                reason: CoverageReason::of(err),
            })
            .chain(
                skipped
                    .iter()
                    .filter(|language| !answered_for.contains(language))
                    .map(|language| Uncovered {
                        language: *language,
                        reason: CoverageReason::NotSearched,
                    }),
            )
            .collect(),
        LiveLookup::NotRun { requested } => requested
            .iter()
            .filter(|language| !answered_for.contains(language))
            .map(|language| Uncovered {
                language: *language,
                reason: CoverageReason::NotIndexed,
            })
            .collect(),
    };
    gaps.sort_by_key(|gap| gap.language.lsp_id());
    gaps.dedup_by_key(|gap| gap.language);
    gaps
}

/// Why a search's `count` is a lower bound rather than the whole match set.
///
/// Each variant is something the command could not see all of, and each has a
/// different remedy, so the flag an agent branches on
/// ([`Section::incomplete`](crate::cli::response::Section)) always travels
/// with a sentence naming which one it was.
///
/// The set is closed over one axis only: whether the answer holds everything
/// ITS SOURCES held. Whether a source is itself current is the other axis and
/// does not belong here — an index-backed count is exact for the build behind
/// it, and a build older than the working tree is what `stale`, `backend:
/// "index"`, and `search index build` are for. Folding the two together would
/// mark every index hit, which is most of them, and a marker that is almost
/// always set is one an agent learns to skip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LowerBound {
    /// The index that answered was built while these paths could not be read,
    /// so its rows are a subset of what its scope claims. They are named
    /// rather than counted because a count leaves the reader nowhere to look
    /// and no way to tell which languages it kept out.
    IndexBuiltOverUnreadPaths {
        paths: Vec<String>,
        /// Whether another build can reach them — asked of the tree when the
        /// bound was made (see [`index_holes_bound`]), because that is the
        /// only thing that settles it. A rebuild reaches a path only if
        /// something can read it now, and prescribing one over a hole that is
        /// still blocked sends a reader around a loop that cannot change the
        /// fact printed beside it.
        repairable: bool,
    },
    /// The scan itself could not read these paths — a file it could not open,
    /// a directory it could not enter, or one whose read failed partway. Named
    /// for the same reason a build's are: a count tells a reader the answer is
    /// short and gives them nowhere to look, and no command can fix a
    /// permission it cannot point at.
    ScanCouldNotReadPaths(Vec<String>),
    /// Live and indexed results overlap while at least one source was not
    /// materialised whole, so their union can be bounded from below but not
    /// counted: deduplication settles the overlap only while both sets are in
    /// hand.
    UnmergedOverlap,
    /// Widening a path query into document symbols stopped at its own cap, so
    /// files it had already reached were never opened.
    LiveWideningCapped,
    /// The index page filled before the index ran out of rows, so what lay
    /// beyond it never reached the answer. Kept apart from the live cap
    /// because the same flag raises both but by different routes.
    IndexPageCapped,
    /// A file the widening reached could not be described by a language
    /// server, so whatever it holds is missing from the answer. Kept apart
    /// from the cap because raising a limit does not make a server answer.
    LiveFileNotDescribed,
    /// This many symbols could not be analysed at all, so they answered none
    /// of the filters the result was narrowed by and are missing from it
    /// rather than having failed them.
    SymbolsNotAnalysed(usize),
    /// Analysis stopped at this many symbols before running out of candidates,
    /// so the ones past the cap answered none of the filters either. Kept
    /// apart from a failed analysis because raising the cap reaches them,
    /// where a failure is what retrying is for.
    AnalysisCapped(usize),
}

impl LowerBound {
    pub fn hint(&self) -> String {
        match self {
            Self::IndexBuiltOverUnreadPaths { paths, .. } => format!(
                "The index was built while {} path(s) could not be read ({}), so anything they \
                 hold is absent from it and the count is a lower bound",
                paths.len(),
                name_some(paths)
            ),
            Self::ScanCouldNotReadPaths(paths) => format!(
                "{} path(s) could not be read while scanning the tree ({}), so anything they hold \
                 is absent from the answer and the count is a lower bound",
                paths.len(),
                name_some(paths)
            ),
            Self::UnmergedOverlap => "Live and indexed results overlap and one of them was not \
                 read whole, so the count is a lower bound on their union"
                .to_string(),
            Self::SymbolsNotAnalysed(symbols) => format!(
                "{symbols} symbol(s) could not be analysed, so they answered none of the filters \
                 applied and are missing from the count rather than failing them"
            ),
            Self::LiveFileNotDescribed => "A file the search widened into could not be described \
                 by its language server, so what it holds is missing from the count"
                .to_string(),
            Self::LiveWideningCapped => "The search stopped widening into more files before it \
                 ran out of them, so the count is a lower bound; raise --limit to widen further"
                .to_string(),
            Self::IndexPageCapped => "The index held more rows than this page asked for, so the \
                 count is a lower bound; raise --limit to reach them"
                .to_string(),
            Self::AnalysisCapped(symbols) => format!(
                "Analysis stopped at {symbols} symbol(s) before running out of candidates, so \
                 those past the cap answered none of the filters applied and are missing from \
                 the count; raise --max-symbols to reach them"
            ),
        }
    }

    pub fn next_command(&self) -> Option<String> {
        match self {
            Self::IndexBuiltOverUnreadPaths { repairable, .. } => {
                repairable.then(|| "symora search index build --force".to_string())
            }
            Self::ScanCouldNotReadPaths(_)
            | Self::UnmergedOverlap
            | Self::LiveWideningCapped
            | Self::IndexPageCapped
            | Self::LiveFileNotDescribed
            | Self::SymbolsNotAnalysed(_)
            | Self::AnalysisCapped(_) => None,
        }
    }
}

/// The stale backing files a page reported, said the way a response says paths.
///
/// A page is a superset of the items an answer emits — ranking, filters, and
/// the limit all cut into it — so a page holding one stale row and one fresh
/// one says nothing about an answer that kept only the fresh one. Comparing
/// against the emitted rows is what narrows the claim to them, and that needs
/// the paths in the form those rows carry.
pub fn relative_stale_files(ctx: &OutputContext, files: &[String]) -> HashSet<String> {
    relative_paths(ctx, files).into_iter().collect()
}

/// Which entry point routed the call to live workspace symbols. An empty
/// result's failure disclosure keys its remedy off this — the honest next
/// command depends on why the index was skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceSearchRoute {
    /// `--workspace-symbols` explicitly skipped the index.
    Forced,
    /// The store reported `NotInitialized`; the live lookup is the
    /// fallback and the zero is workspace-only.
    IndexNotBuilt,
    /// A build owns the index, so it has no completed state to answer
    /// from. The live lookup is the same fallback, but the remedy is to
    /// wait for the build rather than to start one.
    IndexRebuilding,
    /// The store could not be read at all. Like the two above, the live
    /// lookup is the whole answer; unlike them, the state is not one the
    /// index reaches on its own, so what it costs is disclosed separately by
    /// [`index_unavailable_disclosure`].
    IndexUnreadable,
    /// A path-like query routed here; a built index still supplements
    /// the live results in the same call.
    PathQuery,
}

impl WorkspaceSearchRoute {
    /// Whether a built index may supplement the live answer. Only the
    /// path-query route: `--workspace-symbols` is a request to skip the index
    /// — honoring it is what makes the flag a way to get past a stale row —
    /// and the two index-less routes are here precisely because the index has
    /// no completed state to answer from.
    pub fn supplements_from_index(self) -> bool {
        matches!(self, Self::PathQuery)
    }
}

/// How the answer was built, which decides how a gap is worded and what
/// remedy follows it. A route that read the index steers to the surfaces
/// around it; one that could not steers to the reason it could not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisclosureRoute {
    IndexConsulted,
    WorkspaceOnly(WorkspaceSearchRoute),
}

/// Put the shortfall into words and into a remedy. The set is decided in
/// one place and every surface reads it: the structured gaps an agent
/// branches on, the prose, and the follow-up commands cannot name a
/// language or a cause the others do not. What differs is the wording,
/// which turns on how the answer was built.
pub fn symbol_coverage_hints(shortfall: &[Uncovered], route: DisclosureRoute) -> Vec<String> {
    let mut hints: Vec<String> = shortfall
        .iter()
        .map(|gap| {
            let lang = gap.language.lsp_id();
            match gap.reason {
                CoverageReason::NotIndexed => format!(
                    "This result is not authoritative for {lang}: the index does not hold it, and a wildcard query is answered from the index alone"
                ),
                CoverageReason::NotSearched => format!(
                    "This result is not authoritative for {lang}: enough matches came from other languages, so it was never searched — narrow the query with --lang {lang}"
                ),
                reason => match route {
                    DisclosureRoute::IndexConsulted => format!(
                        "This result is not authoritative for {lang}: the index did not answer for it and its language server is unavailable ({reason})",
                        reason = reason.as_str()
                    ),
                    DisclosureRoute::WorkspaceOnly(_) => format!(
                        "This result is not authoritative for {lang}: its workspace symbol lookup failed ({reason})",
                        reason = reason.as_str()
                    ),
                },
            }
        })
        .collect();
    hints.truncate(2);
    hints
}

/// The part of a symbol query a literal text search can look for.
///
/// A path-like query carries structure a symbol lookup understands and a
/// content search does not: `*` and `[` are matched literally there, and `/`
/// separates a container from what it holds. Handing the raw query to
/// `search content` offers a remedy that finds nothing.
pub fn literal_query(query: &str) -> Option<String> {
    let trimmed = query.trim().trim_start_matches('/');
    let last = trimmed.rsplit('/').next().unwrap_or(trimmed);
    let base = last.split('[').next().unwrap_or(last);
    let candidate = base.trim_matches('*').trim();
    (!candidate.is_empty()).then(|| candidate.to_string())
}

/// The remedy for the first gap. A language that was never searched is
/// cured by asking for it, whatever the route; a language that could not
/// answer is cured by the route's own escape — a forced live lookup by
/// dropping the flag, an unbuilt index by building it, but only where the
/// extractor covers the language, since `search index build` can never
/// help one it does not.
pub fn symbol_coverage_next_commands(
    query: &str,
    shortfall: &[Uncovered],
    route: DisclosureRoute,
) -> Vec<String> {
    let Some(gap) = shortfall.first() else {
        return Vec::new();
    };
    let lang = gap.language.lsp_id();
    if gap.reason == CoverageReason::NotSearched {
        return vec![format!("symora search symbols '{query}' --lang {lang}")];
    }
    match route {
        DisclosureRoute::IndexConsulted => literal_query(query)
            .map(|literal| format!("symora search content '{literal}' --lang {lang}"))
            .into_iter()
            .chain(std::iter::once(format!("symora doctor {lang}")))
            .collect(),
        DisclosureRoute::WorkspaceOnly(WorkspaceSearchRoute::Forced) => vec![
            format!("symora search symbols '{query}'"),
            format!("symora doctor {lang}"),
        ],
        // The index cannot be asked to help yet, so the only remedy left is
        // the server's own; what to do about the index is stated by the route
        // — wait for the build, or read the failure `index_unavailable_disclosure`
        // put beside this.
        DisclosureRoute::WorkspaceOnly(
            WorkspaceSearchRoute::IndexRebuilding | WorkspaceSearchRoute::IndexUnreadable,
        ) => {
            vec![format!("symora doctor {lang}")]
        }
        DisclosureRoute::WorkspaceOnly(
            WorkspaceSearchRoute::IndexNotBuilt | WorkspaceSearchRoute::PathQuery,
        ) => {
            if crate::services::store::SymbolExtractor::is_supported(gap.language) {
                vec![
                    "symora search index build".to_string(),
                    format!("symora doctor {lang}"),
                ]
            } else {
                vec![
                    format!("symora search content '{query}' --lang {lang}"),
                    format!("symora doctor {lang}"),
                ]
            }
        }
    }
}

/// How much advice one answer carries. Two facts and two remedies is what an
/// agent acts on; a reader that has to pick from six picks none.
const ADVICE_LIMIT: usize = 2;

/// Lead a section's advice with what makes its count a lower bound.
///
/// The flag, the facts, and the remedies are set in one call because they are
/// one decision: `incomplete` may not be raised without naming the cause, and
/// a bound that gains a remedy must reach every surface that states its fact.
/// Each bound decides for itself whether it has a remedy worth offering — one
/// that cannot act on the fact beside it is withheld at the source, not here.
pub fn with_lower_bounds<T>(section: Section<T>, bounds: &[LowerBound]) -> Section<T> {
    if bounds.is_empty() {
        return section;
    }
    let mut hints: Vec<String> = bounds.iter().map(LowerBound::hint).collect();
    hints.extend(section.hints.iter().cloned());
    let mut next_commands: Vec<String> =
        bounds.iter().filter_map(LowerBound::next_command).collect();
    next_commands.extend(section.next_commands.iter().cloned());
    section
        .with_hints(hints)
        .with_next_commands(next_commands)
        .with_incomplete(true)
}

/// The reasons an answer's count is a lower bound, in the order that decides
/// what to do about them.
///
/// A path THIS RUN could not read and a path the last BUILD could not read
/// look alike, and often they are the same directory — but they are different
/// facts with different remedies, so neither can stand for the other. Both are
/// said, present tense first: what cannot be read now is nearer to what the
/// reader has to do than what could not be read when the index was built, and
/// under a cap it is the one worth keeping. The repetition when they ARE the
/// same directory is the cheaper error — a reader absorbs a fact stated twice.
///
/// Whether the build's hole is still worth a rebuild is not decided here; see
/// [`index_holes_bound`], which asks the tree.
pub fn ordered_bounds(scan: Option<LowerBound>, from_index: Vec<LowerBound>) -> Vec<LowerBound> {
    scan.into_iter().chain(from_index).collect()
}

/// Attach the shortfall's prose and remedy to a section. Every route ends
/// here, so a gap reaches `coverage_gaps` and the prose from the same value
/// and neither can name a language the other does not. The structured field
/// carries every gap; the prose carries as many as the cap allows, leading
/// with whatever changes the answer's meaning most.
///
/// `route_facts` are what the ROUTE has to admit — an index it could not
/// consult, a wildcard matched against the index alone — each with the command
/// that acts on it.
///
/// This is the capped sibling of [`with_lower_bounds`], and the cap is why the
/// advice is assembled as (fact, remedies) entries rather than as two lists:
/// dropping whole entries is what keeps a command from outliving the fact that
/// motivates it. Two lists cut independently would leave a remedy standing
/// under a sentence that does not prescribe it.
pub fn with_coverage_disclosure<T>(
    section: Section<T>,
    shortfall: &[Uncovered],
    query: &str,
    route: DisclosureRoute,
    lower_bounds: &[LowerBound],
    route_facts: &[(String, String)],
) -> Section<T> {
    if shortfall.is_empty() && route_facts.is_empty() && lower_bounds.is_empty() {
        return section;
    }
    // Ordered by how much each changes what the answer means: a count that is
    // a lower bound first, then what the route itself could not do, then what
    // was not covered — and the search's own advice for narrowing last, since
    // it is only worth taking once the reader knows what they are narrowing.
    let mut advice: Vec<(String, Vec<String>)> = lower_bounds
        .iter()
        .map(|bound| (bound.hint(), bound.next_command().into_iter().collect()))
        .collect();
    advice.extend(
        route_facts
            .iter()
            .map(|(fact, remedy)| (fact.clone(), vec![remedy.clone()])),
    );
    let mut gaps = symbol_coverage_hints(shortfall, route).into_iter();
    if let Some(first) = gaps.next() {
        // The remedies answer the first gap, so they travel with it.
        advice.push((
            first,
            symbol_coverage_next_commands(query, shortfall, route),
        ));
        advice.extend(gaps.map(|hint| (hint, Vec::new())));
    }
    advice.truncate(ADVICE_LIMIT);

    let mut hints: Vec<String> = advice.iter().map(|(fact, _)| fact.clone()).collect();
    let mut commands: Vec<String> = advice
        .into_iter()
        .flat_map(|(_, remedies)| remedies)
        .collect();
    // The section's own are not a pair — its hints steer the query and its
    // commands follow the results — so they fill whatever room is left.
    hints.extend(section.hints.iter().cloned());
    commands.extend(section.next_commands.iter().cloned());
    hints.truncate(ADVICE_LIMIT);
    commands.truncate(ADVICE_LIMIT);
    let section = section.with_hints(hints).with_next_commands(commands);
    // Raised, never cleared — same as [`with_lower_bounds`], and same as the
    // early return above. Nothing here knows why a caller marked a count short
    // for a reason that is not a bound.
    match lower_bounds.is_empty() {
        true => section,
        false => section.with_incomplete(true),
    }
}

/// Why an answer is workspace-only, read from the store failure that made it
/// so.
///
/// Every route that falls back to live symbols derives it here, so two
/// surfaces meeting the same store state cannot prescribe opposite remedies —
/// which is what happened while one asserted "the index was consulted" and
/// the other read the outcome. Its pair is
/// [`index_unavailable_disclosure`]: the route decides the wording, and the
/// disclosure says what the failure itself was. `None` where there is no
/// route to take, because the failure is not the store's — see
/// [`StoreError::describes_the_store`].
pub fn workspace_route_for(error: &StoreError) -> Option<WorkspaceSearchRoute> {
    match error {
        StoreError::NotInitialized => Some(WorkspaceSearchRoute::IndexNotBuilt),
        StoreError::Rebuilding => Some(WorkspaceSearchRoute::IndexRebuilding),
        // Nothing to route around: the store never answered, so it is reported
        // as itself (see `StoreError::describes_the_store`).
        StoreError::Unreachable(_) => None,
        // Every other failure leaves the store unread rather than unbuilt.
        // Named rather than caught by a `_` so a variant added to `StoreError`
        // has to be placed here instead of silently reading as unreadable.
        StoreError::Busy
        | StoreError::AlreadyIndexing
        | StoreError::EmptyScope
        | StoreError::Database(_)
        | StoreError::Corrupt(_)
        | StoreError::SchemaMismatch { .. }
        | StoreError::Io(_) => Some(WorkspaceSearchRoute::IndexUnreadable),
    }
}

/// What an index this answer could not consult costs it.
///
/// Not a language gap — nothing is missing from the answer's domain, and each
/// surface already says what DID answer through `backend` and its coverage —
/// but a bare result would otherwise read as one the index confirmed.
/// `NotInitialized` is the exception: an index that was never built is the
/// ordinary state, and the routes that meet it already say so in their own
/// words.
pub fn index_unavailable_disclosure(error: &StoreError) -> Option<(String, String)> {
    match error {
        StoreError::NotInitialized => None,
        StoreError::Rebuilding => Some(index_rebuilding_disclosure()),
        other => Some((
            format!(
                "The search index could not be read, so it took no part in this answer ({other})"
            ),
            "symora search index status".to_string(),
        )),
    }
}

/// The unread paths a build recorded, said the way every other path in a
/// response is said.
///
/// They are stored absolute so a later refresh can match one against the row
/// it repaired; a reader wants them where its own files are.
pub fn relative_unread_paths(ctx: &OutputContext, paths: &[UnreadPath]) -> Vec<UnreadPath> {
    paths
        .iter()
        .map(|unread| UnreadPath {
            path: ctx.relative_path(Path::new(&unread.path)),
            is_file: unread.is_file,
        })
        .collect()
}

/// What a rebuilding index costs an answer that could not consult it. Its
/// remedy is to wait, which is the whole difference from an index that simply
/// could not be read.
pub fn index_rebuilding_disclosure() -> (String, String) {
    (
        "The search index is being rebuilt, so it took no part in this answer".to_string(),
        "symora search index status".to_string(),
    )
}

/// The languages the index's answer speaks for.
///
/// An index that returned nothing speaks for none of them, however wide its
/// build scope: a hit is an answer while a miss is not evidence of absence —
/// a symbol written since the last build is in neither the index nor, without
/// asking, the result. That is why the live lookup then runs for every
/// requested language, and why a failure there leaves a gap even in one the
/// build covers.
///
/// What this deliberately does not do is route on how MANY rows came back. A
/// specific name matches few symbols in any codebase, so a count under the
/// limit is the normal shape of a complete answer, and paying for a live
/// workspace query on every such search is what made the hot path slow.
pub fn vouched_by_index(covered: &[Language], index_answered: bool) -> Vec<Language> {
    if index_answered {
        covered.to_vec()
    } else {
        Vec::new()
    }
}

/// The reason an index-backed count is a lower bound, if it is one.
///
/// A build that could not read some paths left their symbols out of an index
/// whose scope still names their language. That bounds an answer exactly when
/// the answer takes the index as its AUTHORITY for a language — which is the
/// same set that stops the language from appearing in `coverage_gaps`, so the
/// two disclosures are read off one fact. An index that vouched for nothing
/// was not consulted as an authority, and its holes are not this answer's.
///
/// The paths arrive as the build stored them, absolute, because that is what
/// the tree can be asked about; the bound carries them the way a response says
/// paths.
pub fn index_holes_bound(
    ctx: &OutputContext,
    unread_paths: &[UnreadPath],
    answered_from_index: &[Language],
) -> Vec<LowerBound> {
    if unread_paths.is_empty() || answered_from_index.is_empty() {
        return Vec::new();
    }
    // A path that names its own language cannot hold another's, so an answer
    // for languages none of them names is not short on their account.
    if let Some(languages) = languages_behind(unread_paths)
        && !answered_from_index
            .iter()
            .any(|language| languages.contains(language))
    {
        return Vec::new();
    }
    vec![LowerBound::IndexBuiltOverUnreadPaths {
        repairable: unread_paths
            .iter()
            .any(|unread| readable_now(Path::new(&unread.path))),
        paths: relative_unread_paths(ctx, unread_paths)
            .into_iter()
            .map(|unread| unread.path)
            .collect(),
    }]
}

/// Whether a rebuild can clear the hole this path left in the index.
///
/// Asked of the tree, because the tree is what decides it, and asked the way
/// the rebuild's own walk asks, so the answer is about that walk and not about
/// this process. `symlink_metadata` rather than `metadata`: the
/// walk does not follow links, so what one points at is a different path's
/// question, and following one here could lead the probe outside the project
/// or onto a pipe, where an open never returns.
///
/// It is asked rather than inferred from what else the command found: a scan's
/// own shortfall names what IT could not read, and the routes that never scan
/// have no such list — so inferring it would make the advice depend on which
/// route asked rather than on the tree, and `--lang`, which only narrows a
/// search, would decide whether a rebuild is prescribed.
fn readable_now(path: &Path) -> bool {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        // Gone, so the next build finds nothing there to miss.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
        // Not even its name can be read, so neither can the build.
        Err(_) => return false,
    };
    if meta.is_dir() {
        std::fs::read_dir(path).is_ok()
    } else if meta.is_file() {
        // Read, not merely open: a file that opens and then fails to read is
        // recorded as a hole again, so the rebuild would leave the disclosure
        // exactly where it found it. One byte, because reading the file is the
        // work the remedy exists to do — a mount that fails partway is still
        // called repairable, and the cost of that is one rebuild, where the
        // cost of reading every hole here is paid on every search.
        std::fs::File::open(path).is_ok_and(|mut file| file.read(&mut [0]).is_ok())
    } else {
        // A link or a special file: the walk descends neither and reads
        // neither, so the next build clears the hole whatever this process
        // could do with it. Opening one is also how a probe hangs.
        true
    }
}
/// Name the paths a disclosure is about, keeping the sentence bounded.
///
/// A build can be turned away from more paths than a hint can hold, and a hint
/// nobody reads to the end is a hint that names nothing. Enough of them to act
/// on, then the count of the rest.
pub fn name_some(paths: &[String]) -> String {
    const NAMED: usize = 3;
    let named = paths.iter().take(NAMED).cloned().collect::<Vec<_>>();
    match paths.len().saturating_sub(named.len()) {
        0 => named.join(", "),
        rest => format!("{} and {rest} more", named.join(", ")),
    }
}

/// The languages an index's unread paths can be keeping matches from, or
/// `None` when they could be keeping matches from any of them.
///
/// A FILE's name settles its language, and the index derives a file's language
/// from that same name — so a `.py` file a build could not open leaves no Rust
/// row missing, and one whose name names no language leaves no row missing at
/// all. Anything the walk could not ENTER is not a file it read a name for: it
/// can hold any language, and one of those is enough to leave every language
/// in doubt.
fn languages_behind(unread_paths: &[UnreadPath]) -> Option<Vec<Language>> {
    let mut languages = Vec::new();
    for unread in unread_paths {
        // Not a file the walk read a name for, so its name settles nothing.
        if !unread.is_file {
            return None;
        }
        match Language::from_path(Path::new(&unread.path)) {
            // A name that names no language keeps no language's rows out: the
            // index would have had no symbol row for such a file either.
            Language::Unknown => {}
            language => {
                if !languages.contains(&language) {
                    languages.push(language);
                }
            }
        }
    }
    Some(languages)
}

/// Paths as a disclosure carries them: one string each, in a stable order so
/// two runs over the same tree word the shortfall alike.
pub fn as_paths(paths: &[PathBuf]) -> Vec<String> {
    let mut named: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    named.sort();
    named.dedup();
    named
}

/// The scan's own unread paths, said the way every other path in a response is.
pub fn relative_paths(ctx: &OutputContext, paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .map(|path| ctx.relative_path(Path::new(path)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An index that did not answer took no part in the answer, and each
    /// reason it did not has its own remedy. Read from the store outcome
    /// here — a surface that asserted the route instead prescribed narrowing
    /// a result the index had never produced.
    #[test]
    fn an_index_that_did_not_answer_never_reads_as_one_that_did() {
        let shortfall = [Uncovered {
            language: Language::Rust,
            reason: CoverageReason::ServerNotInstalled,
        }];
        let commands = |error: &StoreError| {
            symbol_coverage_next_commands(
                "alpha",
                &shortfall,
                DisclosureRoute::WorkspaceOnly(
                    workspace_route_for(error).expect("a store state has a route"),
                ),
            )
        };
        let build = "symora search index build".to_string();

        assert!(
            commands(&StoreError::NotInitialized).contains(&build),
            "an index that was never built is cured by building it"
        );
        for out_of_reach in [
            StoreError::Rebuilding,
            StoreError::Corrupt("boom".to_string()),
            StoreError::Busy,
            StoreError::Io(std::io::Error::other("boom")),
        ] {
            assert!(
                !commands(&out_of_reach).contains(&build),
                "{out_of_reach} is not cured by starting a build"
            );
            assert_ne!(
                commands(&out_of_reach),
                symbol_coverage_next_commands("alpha", &shortfall, DisclosureRoute::IndexConsulted),
                "{out_of_reach} left the index out of the answer, so it may not be steered \
                 as though the index had narrowed one"
            );
        }
    }

    /// `--workspace-symbols` is how a caller gets an answer that owes nothing
    /// to the index — including a stale row the index still holds for a
    /// symbol the working tree no longer has. A route that supplements it
    /// anyway returns exactly the data the flag was used to avoid.
    #[test]
    fn only_a_path_query_lets_the_index_supplement_a_live_answer() {
        assert!(WorkspaceSearchRoute::PathQuery.supplements_from_index());
        assert!(!WorkspaceSearchRoute::Forced.supplements_from_index());
        assert!(!WorkspaceSearchRoute::IndexNotBuilt.supplements_from_index());
        assert!(!WorkspaceSearchRoute::IndexRebuilding.supplements_from_index());
        assert!(!WorkspaceSearchRoute::IndexUnreadable.supplements_from_index());
    }

    /// A route that asked the index and was refused is no longer the route
    /// that asked. `PathQuery` prescribes a build, which is exactly wrong for
    /// an index already building or one nothing can open — so the route a
    /// failed supplement leaves behind is always derived from the failure, and
    /// `workspace_route_for` can never answer with a route that would ask
    /// again.
    #[test]
    fn a_refused_supplement_never_leaves_the_route_that_asked_for_it() {
        let failures: Vec<fn() -> StoreError> = vec![
            || StoreError::NotInitialized,
            || StoreError::Rebuilding,
            || StoreError::Busy,
            || StoreError::Corrupt("boom".to_string()),
            || StoreError::Io(std::io::Error::other("boom")),
        ];
        for make in failures {
            let route = workspace_route_for(&make()).expect("a store state has a route");
            assert!(
                !route.supplements_from_index(),
                "{} left a route that would ask the index again",
                make()
            );
        }

        // A transport failure is not a store state, so it has no route at all
        // — it is reported as itself rather than answered around.
        assert!(
            workspace_route_for(&StoreError::Unreachable(Box::new(
                crate::error::LspError::NotConnected
            )))
            .is_none()
        );
    }

    /// A build's holes and this run's holes can be different directories: one
    /// blocked while the index was built and since fixed, another blocked
    /// only now. Collapsing them to the build's statement leaves `--force` as
    /// the only suggestion, and `--force` cannot read a path that is blocked
    /// today — it just builds another holed index. The present-tense fact
    /// leads because it is the one with something to do about it.
    #[test]
    fn a_present_hole_is_never_spoken_for_by_a_past_one() {
        let from_index = vec![LowerBound::IndexBuiltOverUnreadPaths {
            paths: vec!["src/one".to_string()],
            repairable: true,
        }];
        let from_detection = Some(LowerBound::ScanCouldNotReadPaths(vec![
            "src/p0".to_string(),
        ]));

        assert_eq!(
            ordered_bounds(from_detection.clone(), from_index.clone()),
            vec![
                LowerBound::ScanCouldNotReadPaths(vec!["src/p0".to_string()]),
                LowerBound::IndexBuiltOverUnreadPaths {
                    paths: vec!["src/one".to_string()],
                    repairable: true,
                }
            ],
            "both are said, and what cannot be read now comes first"
        );
        assert_eq!(
            ordered_bounds(None, from_index.clone()),
            from_index,
            "a build's holes still stand on their own"
        );
        assert_eq!(
            ordered_bounds(from_detection.clone(), Vec::new()),
            Vec::from_iter(from_detection)
        );
        assert!(ordered_bounds(None, Vec::new()).is_empty());
    }

    /// A build that could not read some paths leaves an index short for
    /// languages its scope still names. That bounds an answer only where the
    /// index was the authority — a language looked up live instead does not
    /// inherit the index's holes, and neither does one nothing was published
    /// from.
    /// A context rooted where the paths are relative to, so a bound names them
    /// the way a response does.
    fn ctx_at(root: &std::path::Path) -> crate::cli::OutputContext {
        crate::cli::OutputContext::new(root.to_path_buf(), Default::default())
    }

    fn dir(path: &str) -> UnreadPath {
        UnreadPath {
            path: path.to_string(),
            is_file: false,
        }
    }

    fn file(path: &str) -> UnreadPath {
        UnreadPath {
            path: path.to_string(),
            is_file: true,
        }
    }

    #[test]
    fn an_index_built_over_unread_paths_bounds_only_what_it_answered_for() {
        let ctx = ctx_at(std::path::Path::new("/"));
        let blocked = vec![dir("src/blocked"), file("src/b.rs")];
        let bound = index_holes_bound(&ctx, &blocked, &[Language::Rust]);
        assert!(matches!(
            bound.as_slice(),
            [LowerBound::IndexBuiltOverUnreadPaths { paths, .. }]
                if paths == &["src/blocked".to_string(), "src/b.rs".to_string()]
        ));
        assert!(index_holes_bound(&ctx, &[], &[Language::Rust]).is_empty());
        assert!(
            index_holes_bound(&ctx, &blocked, &[]).is_empty(),
            "an answer that looked every language up live rests on no claim of the index's"
        );
    }

    /// A path names its own language, and the index derives a file's language
    /// from the same name — so a build turned away from Python files left no
    /// Rust row missing, and saying otherwise sends an agent to rebuild over a
    /// hole that was never in its way. A path that names no language (a
    /// directory, or a file without an extension) could hold any of them, and
    /// one of those puts every language back in doubt.
    /// A FILE's name settles its language, because the index derives a file's
    /// language from the same name. A directory's name settles nothing, and
    /// nothing can re-derive the difference later — the path is unreadable
    /// then too — which is why the walk records which it saw. Read off the
    /// name alone, a directory called `generated.py` would be taken for Python
    /// and a Rust answer would go unqualified.
    #[test]
    fn a_hole_bounds_only_the_languages_its_paths_could_hold() {
        let ctx = ctx_at(std::path::Path::new("/"));
        assert!(
            index_holes_bound(&ctx, &[file("src/a.py")], &[Language::Rust]).is_empty(),
            "a Python file cannot be a Rust answer's shortfall"
        );
        assert_eq!(
            index_holes_bound(&ctx, &[file("src/a.py")], &[Language::Python]).len(),
            1,
            "it is exactly the Python answer's shortfall"
        );
        assert!(
            index_holes_bound(&ctx, &[file("src/notes")], &[Language::Rust]).is_empty(),
            "a file whose name names no language has no symbol row to be missing"
        );
        assert_eq!(
            index_holes_bound(&ctx, &[dir("src/generated.py")], &[Language::Rust]).len(),
            1,
            "a directory could hold anything, whatever it is called"
        );
        assert_eq!(
            index_holes_bound(
                &ctx,
                &[file("src/a.py"), dir("src/blocked")],
                &[Language::Rust]
            )
            .len(),
            1,
            "one unattributable path is enough"
        );
    }

    /// The cap drops whole entries. A remedy that outlives the sentence
    /// explaining it is a command an agent runs against a fact it was never
    /// told: a rebuild prescribed under two hints that both say the tree
    /// cannot be read.
    #[test]
    fn a_cap_never_leaves_a_remedy_without_its_fact() {
        let bounds = vec![
            LowerBound::ScanCouldNotReadPaths(vec!["src/blocked".to_string()]),
            LowerBound::IndexBuiltOverUnreadPaths {
                paths: vec!["src/blocked".to_string()],
                repairable: false,
            },
        ];
        let wildcard = [(
            "Wildcards are matched against the index alone".to_string(),
            "symora search index build".to_string(),
        )];
        let section = with_coverage_disclosure(
            Section::with_total(Vec::<()>::new(), 0),
            &[],
            "zzz*",
            DisclosureRoute::IndexConsulted,
            &bounds,
            &wildcard,
        );

        assert_eq!(section.hints.len(), ADVICE_LIMIT, "{:?}", section.hints);
        assert!(
            section.next_commands.is_empty(),
            "the wildcard's fact did not fit, so neither does its remedy: {:?}",
            section.next_commands
        );
        assert!(section.incomplete);

        // With room for it, the pair arrives whole.
        let section = with_coverage_disclosure(
            Section::with_total(Vec::<()>::new(), 0),
            &[],
            "zzz*",
            DisclosureRoute::IndexConsulted,
            &bounds[..1],
            &wildcard,
        );
        assert_eq!(section.hints.len(), 2);
        assert_eq!(
            section.next_commands,
            vec!["symora search index build".to_string()]
        );
    }

    /// A rebuild reaches a path only if something can read it now, so whether
    /// `--force` is worth offering is a question about the tree — asked of the
    /// tree, and not inferred from what else the same command happened to
    /// walk. The fact is stated either way; only the command that cannot act
    /// on it is withheld.
    #[test]
    fn a_remedy_the_tree_has_disproved_is_not_offered() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("blocked")).unwrap();
        std::fs::write(root.join("blocked/a.rs"), "fn a() {}\n").unwrap();
        std::fs::create_dir(root.join("open")).unwrap();
        std::fs::set_permissions(root.join("blocked"), std::fs::Permissions::from_mode(0o000))
            .unwrap();

        // Probed against the tree, never read off the value under test: a
        // guard that consults the answer takes a defect for the environment.
        let mode_bites = std::fs::read_dir(root.join("blocked")).is_err();

        let ctx = ctx_at(root);
        let dir_at = |name: &str| UnreadPath {
            path: root.join(name).display().to_string(),
            is_file: false,
        };
        let commands = |unread: &[UnreadPath]| {
            index_holes_bound(&ctx, unread, &[Language::Rust])
                .iter()
                .filter_map(LowerBound::next_command)
                .collect::<Vec<_>>()
        };

        let over_a_blocked_path = commands(&[dir_at("blocked")]);
        let over_a_readable_path = commands(&[dir_at("open")]);
        let over_a_vanished_path = commands(&[dir_at("gone")]);
        let over_both = commands(&[dir_at("blocked"), dir_at("open")]);
        std::fs::set_permissions(root.join("blocked"), std::fs::Permissions::from_mode(0o755))
            .unwrap();

        assert_eq!(
            over_a_readable_path,
            vec!["symora search index build --force".to_string()],
            "the hole is reachable now, so a rebuild clears it"
        );
        assert_eq!(
            over_a_vanished_path,
            vec!["symora search index build --force".to_string()],
            "a path that is gone leaves the next build nothing to miss"
        );
        if mode_bites {
            assert!(
                over_a_blocked_path.is_empty(),
                "a rebuild has no more way in than this probe did"
            );
            assert_eq!(
                over_both.len(),
                1,
                "one reachable hole is worth a rebuild even beside one that is not"
            );
        }

        // The fact is stated whether or not the remedy is.
        let bound = index_holes_bound(&ctx, &[dir_at("open")], &[Language::Rust]);
        assert!(bound[0].hint().contains("could not be read"));
    }

    #[test]
    fn coverage_reason_classifies_method_not_found_as_unsupported() {
        // A server that does not implement workspace/symbol returns -32601 —
        // permanent, so it must read as unsupported, not a retryable failure.
        let reason = |err| CoverageReason::of(&err).as_str();
        assert_eq!(
            reason(LspError::ServerError {
                code: -32601,
                message: "method not found".to_string(),
            }),
            "unsupported"
        );
        assert_eq!(reason(LspError::Timeout("slow".to_string())), "timed_out");
        assert_eq!(
            reason(LspError::ServerNotInstalled {
                name: "x".to_string(),
                install_hint: "y".to_string(),
            }),
            "server_not_installed"
        );
        // Any other server error stays the generic catch-all.
        assert_eq!(
            reason(LspError::ServerError {
                code: -32603,
                message: "internal".to_string(),
            }),
            "unavailable"
        );
    }
}
