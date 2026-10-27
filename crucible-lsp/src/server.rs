//! Document state, per-pack compiles, publishing, and the request handlers.
//!
//! ```text
//!  edit ──► parse + lint (this file) ──► publish
//!                │
//!                └─► compile the pack's entry (blocking pool) ──► publish every file it names
//! ```

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    CompletionOptions, CompletionParams, CompletionResponse, Diagnostic,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionParams,
    GotoDefinitionResponse, Hover, HoverContents, HoverParams, HoverProviderCapability,
    InitializeParams, InitializeResult, Location, MarkupContent, MarkupKind, OneOf,
    ReferenceParams, SaveOptions, ServerCapabilities, ServerInfo, SignatureHelp,
    SignatureHelpOptions, SignatureHelpParams, TextDocumentSyncCapability, TextDocumentSyncKind,
    TextDocumentSyncOptions, TextDocumentSyncSaveOptions, Uri,
};
use tower_lsp_server::{Client, LanguageServer};

use crate::analyze::{self, canonical};
use crate::check::{self, Finding, Origin};
use crate::complete::Completer;
use crate::docs::{self, Reference};
use crate::index::Index;
use crate::lint;
use crate::navigate::{self, Navigator};
use crate::snapshot::{PackContext, Snapshot};
use crate::text::Text;

pub struct Backend {
    client: Client,
    reference: Reference,
    state: Mutex<State>,
    snippets: AtomicBool,
}

#[derive(Default)]
struct State {
    docs: HashMap<Uri, Doc>,
    packs: HashMap<PathBuf, PackState>,
}

struct Doc {
    version: i32,
    path: Option<PathBuf>,
    canonical: Option<PathBuf>,
    text: Arc<Text>,
    /// The last version that parsed, with the text its spans index.
    parsed: Option<(Arc<Index>, Arc<Text>)>,
    /// Parse error or lints of the current text.
    local: Vec<Finding>,
    parse_failed: bool,
    context: Arc<PackContext>,
}

#[derive(Default)]
struct PackState {
    generation: u64,
    compiled: BTreeMap<PathBuf, Vec<Finding>>,
    /// Files last published with diagnostics, so a fix clears them.
    published: BTreeSet<PathBuf>,
}

struct CompileJob {
    pack: PathBuf,
    entry: PathBuf,
    source: String,
    generation: u64,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Backend {
            client,
            reference: Reference::new(),
            state: Mutex::new(State::default()),
            snippets: AtomicBool::new(false),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Open documents' text by canonical path, falling back to disk.
    fn reader(&self) -> impl Fn(&Path) -> Option<String> + use<> {
        let open: HashMap<PathBuf, Arc<Text>> = self
            .state()
            .docs
            .values()
            .filter_map(|doc| Some((doc.canonical.clone()?, doc.text.clone())))
            .collect();
        move |path: &Path| match open.get(&canonical(path)) {
            Some(text) => Some(text.as_str().to_owned()),
            None => std::fs::read_to_string(path).ok(),
        }
    }

    fn snapshot(&self, uri: &Uri) -> Option<Snapshot> {
        let state = self.state();
        let doc = state.docs.get(uri)?;
        Some(Snapshot::new(
            doc.path.clone(),
            doc.text.clone(),
            doc.parsed.as_ref().map(|(index, _)| index.clone()),
            doc.context.clone(),
        ))
    }

    async fn refresh(&self, uri: Uri) {
        let job = self.analyze(&uri);
        let pack = job.as_ref().map(|job| job.pack.clone()).or_else(|| {
            let state = self.state();
            let doc = state.docs.get(&uri)?;
            Some(doc.context.pack.as_ref()?.dir.clone())
        });
        match &pack {
            Some(pack) => self.publish_pack(pack).await,
            None => self.publish_alone(&uri).await,
        }
        let Some(job) = job else { return };
        let CompileJob {
            pack,
            entry,
            source,
            generation,
        } = job;
        let Ok(found) = tokio::task::spawn_blocking(move || check::compile(&entry, &source)).await
        else {
            return;
        };
        {
            let mut state = self.state();
            let packs = state.packs.entry(pack.clone()).or_default();
            if packs.generation != generation {
                return;
            }
            packs.compiled = found;
        }
        self.publish_pack(&pack).await;
    }

    /// Parse and lint the document, store the result, and say what to compile.
    fn analyze(&self, uri: &Uri) -> Option<CompileJob> {
        let (version, text, path) = {
            let state = self.state();
            let doc = state.docs.get(uri)?;
            (doc.version, doc.text.clone(), doc.path.clone())
        };
        let read = self.reader();
        let analysis = analyze::analyze(&self.reference, path.as_deref(), text.as_str(), &read);
        let mut state = self.state();
        let doc = state.docs.get_mut(uri)?;
        if doc.version != version {
            return None;
        }
        doc.local = analysis.local;
        doc.parse_failed = analysis.index.is_none();
        match analysis.index {
            Some(index) => {
                doc.parsed = Some((Arc::new(index), text));
                doc.context = Arc::new(analysis.context);
            }
            None if doc.context.pack.is_none() => doc.context = Arc::new(analysis.context),
            None => {}
        }
        let compile = analysis.compile?;
        let packs = state.packs.entry(compile.pack.clone()).or_default();
        packs.generation += 1;
        Some(CompileJob {
            pack: compile.pack,
            entry: compile.entry,
            source: compile.source,
            generation: packs.generation,
        })
    }

    /// Publish every file of `pack` that has, or had, a diagnostic.
    async fn publish_pack(&self, pack: &Path) {
        let read = self.reader();
        let batches: Vec<(Uri, Vec<Diagnostic>)> = {
            let mut state = self.state();
            let state = &mut *state;
            let packs = state.packs.entry(pack.to_path_buf()).or_default();
            let mut files: BTreeSet<PathBuf> = packs.compiled.keys().cloned().collect();
            files.extend(packs.published.iter().cloned());
            let open: Vec<(&Uri, &Doc)> = state
                .docs
                .iter()
                .filter(|(_, doc)| doc.context.pack.as_ref().is_some_and(|p| p.dir == pack))
                .collect();
            files.extend(open.iter().filter_map(|(_, doc)| doc.canonical.clone()));
            let mut published = BTreeSet::new();
            let batches = files
                .into_iter()
                .filter_map(|file| {
                    let doc = open
                        .iter()
                        .find(|(_, doc)| doc.canonical.as_ref() == Some(&file));
                    let uri = match doc {
                        Some((uri, _)) => (*uri).clone(),
                        None => Uri::from_file_path(&file)?,
                    };
                    let text = match doc {
                        Some((_, doc)) => doc.text.clone(),
                        None => Arc::new(Text::new(read(&file).unwrap_or_default())),
                    };
                    let local = doc.map(|(_, doc)| doc.local.as_slice()).unwrap_or_default();
                    let compiled = match doc {
                        Some((_, doc)) if doc.parse_failed => &[][..],
                        _ => packs
                            .compiled
                            .get(&file)
                            .map(Vec::as_slice)
                            .unwrap_or_default(),
                    };
                    let diagnostics = merge(local, compiled, &text);
                    if !diagnostics.is_empty() {
                        published.insert(file);
                    }
                    Some((uri, diagnostics))
                })
                .collect();
            packs.published = published;
            batches
        };
        for (uri, diagnostics) in batches {
            self.client
                .publish_diagnostics(uri, diagnostics, None)
                .await;
        }
    }

    /// Publish a document that belongs to no pack: its own findings only.
    async fn publish_alone(&self, uri: &Uri) {
        let diagnostics = {
            let state = self.state();
            let Some(doc) = state.docs.get(uri) else {
                return;
            };
            merge(&doc.local, &[], &doc.text)
        };
        self.client
            .publish_diagnostics(uri.clone(), diagnostics, None)
            .await;
    }
}

/// A file's diagnostics: the compiler's, then the lints that do not repeat one of them.
fn merge(local: &[Finding], compiled: &[Finding], text: &Text) -> Vec<Diagnostic> {
    let compiled: Vec<Diagnostic> = compiled.iter().map(|f| f.diagnostic(text)).collect();
    let local = local.iter().filter(|finding| {
        finding.origin == Origin::Compiler
            || !lint::SHADOWS_COMPILE.contains(&finding.code.as_str())
            || !compiled
                .iter()
                .any(|c| check::overlaps(&c.range, &finding.range(text)))
    });
    compiled
        .iter()
        .cloned()
        .chain(local.map(|f| f.diagnostic(text)))
        .collect()
}

impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let snippets = params
            .capabilities
            .text_document
            .as_ref()
            .and_then(|t| t.completion.as_ref())
            .and_then(|c| c.completion_item.as_ref())
            .and_then(|i| i.snippet_support)
            .unwrap_or(false);
        self.snippets.store(snippets, Ordering::Relaxed);
        let triggers = |chars: &[&str]| Some(chars.iter().map(|c| (*c).to_owned()).collect());
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(false),
                        })),
                        ..TextDocumentSyncOptions::default()
                    },
                )),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: triggers(&[".", "\"", "'", "/", "(", ","]),
                    ..CompletionOptions::default()
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: triggers(&["(", ","]),
                    retrigger_characters: triggers(&["="]),
                    ..SignatureHelpOptions::default()
                }),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                ..ServerCapabilities::default()
            },
            server_info: Some(ServerInfo {
                name: "crucible-lsp".to_owned(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
            ..InitializeResult::default()
        })
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let document = params.text_document;
        let path = document.uri.to_file_path().map(|p| p.into_owned());
        let doc = Doc {
            version: document.version,
            canonical: path.as_deref().map(canonical),
            path,
            text: Arc::new(Text::new(document.text)),
            parsed: None,
            local: Vec::new(),
            parse_failed: false,
            context: Arc::default(),
        };
        self.state().docs.insert(document.uri.clone(), doc);
        self.refresh(document.uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let Some(change) = params.content_changes.into_iter().last() else {
            return;
        };
        {
            let mut state = self.state();
            let Some(doc) = state.docs.get_mut(&uri) else {
                return;
            };
            if params.text_document.version < doc.version {
                return;
            }
            doc.version = params.text_document.version;
            doc.text = Arc::new(Text::new(change.text));
        }
        self.refresh(uri).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri;
        let others: Vec<Uri> = {
            let state = self.state();
            let pack = state
                .docs
                .get(&uri)
                .and_then(|doc| Some(doc.context.pack.as_ref()?.dir.clone()));
            state
                .docs
                .iter()
                .filter(|(other, doc)| {
                    *other != &uri
                        && pack.is_some()
                        && doc.context.pack.as_ref().map(|p| &p.dir) == pack.as_ref()
                })
                .map(|(other, _)| other.clone())
                .collect()
        };
        self.refresh(uri).await;
        for other in others {
            self.refresh(other).await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        let pack = self
            .state()
            .docs
            .remove(&uri)
            .and_then(|doc| Some(doc.context.pack.as_ref()?.dir.clone()));
        match pack {
            Some(pack) => self.publish_pack(&pack).await,
            None => self.client.publish_diagnostics(uri, Vec::new(), None).await,
        }
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let position = params.text_document_position;
        let Some(snapshot) = self.snapshot(&position.text_document.uri) else {
            return Ok(None);
        };
        let read = self.reader();
        let completer = Completer {
            reference: &self.reference,
            snapshot: &snapshot,
            snippets: self.snippets.load(Ordering::Relaxed),
            read: &read,
        };
        let offset = snapshot.text.offset(position.position);
        Ok(Some(CompletionResponse::Array(completer.complete(offset))))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let position = params.text_document_position_params;
        let Some(snapshot) = self.snapshot(&position.text_document.uri) else {
            return Ok(None);
        };
        let read = self.reader();
        let navigator = Navigator {
            reference: &self.reference,
            snapshot: &snapshot,
            read: &read,
        };
        let offset = snapshot.text.offset(position.position);
        Ok(navigator.hover(offset).map(|(value, range)| Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: Some(snapshot.text.range(range)),
        }))
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let position = params.text_document_position_params;
        let Some(snapshot) = self.snapshot(&position.text_document.uri) else {
            return Ok(None);
        };
        let offset = snapshot.text.offset(position.position);
        let structure = &snapshot.structure;
        if structure
            .string_at(offset)
            .is_some_and(|s| s.frame.is_none())
        {
            return Ok(None);
        }
        let Some(call) = structure.call_at(offset) else {
            return Ok(None);
        };
        let Some(callee) = structure.frame(call).and_then(|f| f.callee.as_deref()) else {
            return Ok(None);
        };
        let shadowed = snapshot
            .index
            .as_ref()
            .is_some_and(|index| index.resolve(callee, offset).is_some());
        let Some(function) = self.reference.function(callee).filter(|_| !shadowed) else {
            return Ok(None);
        };
        let active = structure
            .slot_at(call, offset)
            .and_then(|slot| slot.key.as_ref())
            .map(|key| key.name.as_str());
        Ok(Some(docs::signature_help(function, active)))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let position = params.text_document_position_params;
        let Some(snapshot) = self.snapshot(&position.text_document.uri) else {
            return Ok(None);
        };
        let read = self.reader();
        let navigator = Navigator {
            reference: &self.reference,
            snapshot: &snapshot,
            read: &read,
        };
        let offset = snapshot.text.offset(position.position);
        let Some(target) = navigator.definition(offset) else {
            return Ok(None);
        };
        let here = snapshot.path.as_deref().map(canonical);
        let (uri, text) = if here.as_deref() == Some(canonical(&target.path).as_path()) {
            (position.text_document.uri, snapshot.text.clone())
        } else {
            let Some(uri) = Uri::from_file_path(&target.path) else {
                return Ok(None);
            };
            (
                uri,
                Arc::new(Text::new(read(&target.path).unwrap_or_default())),
            )
        };
        Ok(Some(GotoDefinitionResponse::Scalar(Location {
            uri,
            range: text.range(target.bytes),
        })))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let position = params.text_document_position;
        let Some(snapshot) = self.snapshot(&position.text_document.uri) else {
            return Ok(None);
        };
        let read = self.reader();
        let navigator = Navigator {
            reference: &self.reference,
            snapshot: &snapshot,
            read: &read,
        };
        let offset = snapshot.text.offset(position.position);
        let found = navigator
            .references(offset, params.context.include_declaration)
            .into_iter()
            .map(|at| Location {
                uri: position.text_document.uri.clone(),
                range: snapshot.text.range(at),
            })
            .collect();
        Ok(Some(found))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let state = self.state();
        let Some((index, text)) = state
            .docs
            .get(&params.text_document.uri)
            .and_then(|doc| doc.parsed.clone())
        else {
            return Ok(None);
        };
        drop(state);
        Ok(Some(DocumentSymbolResponse::Nested(navigate::symbols(
            &index, &text,
        ))))
    }
}
