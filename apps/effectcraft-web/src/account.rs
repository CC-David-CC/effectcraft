//! WebOS account project startup and the browser's immutable media table.

use std::{cell::{Cell, RefCell}, collections::BTreeSet, rc::Rc};

use effectcraft_engine::{Session, cmd::PreparedAccountSave, project::{Footage, FootageKind, ItemKind}};
use effectcraft_ui_egui::{AccountProjectFn, AccountProjectRequest, DialogDone, EffectcraftApp, FileDialog};
use serde_json::{Value, json};
use wasm_bindgen::JsValue;

use crate::{
    account_port::{AccountPort, field, string},
    files,
};

pub(crate) struct AccountView {
    port: AccountPort,
    action: Rc<RefCell<Option<AccountProjectRequest>>>,
    last_state: Value,
    pending: Option<Save>,
    dialog: Option<(String, Option<u64>)>,
    media_pending: Rc<Cell<bool>>,
    sequence: u64,
    lease: Option<String>,
    finalized: bool,
}

struct Save {
    ticket: String,
    document: String,
    revision: String,
    path: String,
    project: PreparedAccountSave,
}

impl AccountView {
    pub(crate) fn new(port: AccountPort) -> Self {
        Self {
            port,
            action: Rc::default(),
            last_state: Value::Null,
            pending: None,
            dialog: None,
            media_pending: Rc::default(),
            sequence: 0,
            lease: None,
            finalized: false,
        }
    }

    pub(crate) fn project_service(&self) -> AccountProjectFn {
        let action = self.action.clone();
        Box::new(move |request| {
            let mut next = action.try_borrow_mut().map_err(|_| "EffectCraft is processing a project request.")?;
            if next.is_some() {
                return Err("EffectCraft is processing a project request.".into());
            }
            *next = Some(request);
            Ok(())
        })
    }

    pub(crate) fn suspended(&self) -> bool {
        self.lease.is_some() || self.finalized
    }

    /// Complete the native command after its selected originals have reached Files.
    pub(crate) fn file_dialog(&self) -> impl Fn(FileDialog, DialogDone) + 'static {
        let port = self.port.clone();
        let pending = self.media_pending.clone();
        move |request, done| {
            let (exts, many) = match request {
                FileDialog::Open { exts, many } => (exts, many),
                FileDialog::Save { default, .. } => {
                    done(Ok(crate::app::download_path(&default).into_iter().collect()));
                    return;
                }
                FileDialog::Folder | FileDialog::OpenProject => {
                    done(Err("Use Files to choose an account folder or project.".into()));
                    return;
                }
            };
            if pending.replace(true) {
                done(Err("Finish the current EffectCraft media import first.".into()));
                return;
            }
            let accept = exts.iter().map(|ext| format!(".{ext}")).collect::<Vec<_>>().join(",");
            let port = port.clone();
            let pending = pending.clone();
            files::choose_raw(&accept, many, move |selection| {
                wasm_bindgen_futures::spawn_local(async move {
                    let paths = match selection {
                        Ok(picked) => store_originals(&port, picked).await,
                        Err(error) => Err(error),
                    };
                    done(paths);
                    pending.set(false);
                    crate::repaint();
                });
            });
        }
    }

    pub(crate) fn media_picker(&self) -> impl Fn(&[&str]) -> Vec<String> + 'static {
        let port = self.port.clone();
        let pending = self.media_pending.clone();
        move |extensions| {
            let accept = extensions.iter().map(|extension| format!(".{extension}")).collect::<Vec<_>>().join(",");
            let port = port.clone();
            let pending = pending.clone();
            if let Err(error) = files::pick_raw(&accept, true, move |files| import_media(port, pending, files)) {
                log::warn!("EffectCraft media picker: {error:?}");
            }
            Vec::new()
        }
    }

    pub(crate) fn import_dropped(&self, files: Vec<web_sys::File>) {
        import_media(self.port.clone(), self.media_pending.clone(), files);
    }

    fn busy(&self) -> bool {
        self.pending.is_some() || self.dialog.is_some() || self.media_pending.get()
    }

    fn unfinished(app: &EffectcraftApp) -> bool {
        app.session.state.text_edit.is_some()
    }

    fn state(&self, app: &EffectcraftApp) -> Value {
        let project = &app.session;
        let unfinished = Self::unfinished(app);
        json!({"available":true,"documentId":project.script_ui.project_epoch.to_string(),"revision":project.revision.to_string(),
            "recoveryRevision":project.revision.to_string(),"dirty":project.is_dirty(),"capturable":!unfinished,
            "unfinishedEdit":unfinished,"autosaveEligible":false,"operationPending":self.busy()})
    }

    fn targets(app: &EffectcraftApp, packet: &JsValue) -> bool {
        number(packet, "documentId") == Some(app.session.script_ui.project_epoch) && number(packet, "revision") == Some(app.session.revision)
    }

    pub(crate) fn poll(&mut self, app: &mut EffectcraftApp, ctx: &egui::Context) {
        for (kind, packet) in self.port.take_messages() {
            if let Err(error) = self.receive(app, ctx, &kind, &packet) {
                app.ui.status = error;
            }
        }
        let action = self.action.borrow_mut().take();
        if let Some(action) = action
            && let Err(error) = self.project_action(app, action)
        {
            app.ui.status = error;
        }
    }

    pub(crate) fn publish_state(&mut self, app: &EffectcraftApp) -> Result<(), String> {
        let state = self.state(app);
        if state != self.last_state {
            self.port.publish("project-state", state.clone(), None)?;
            self.last_state = state;
        }
        Ok(())
    }

    fn receive(&mut self, app: &mut EffectcraftApp, ctx: &egui::Context, kind: &str, packet: &JsValue) -> Result<(), String> {
        if kind == "save-result" {
            return self.settle_save(app, packet);
        }
        let Some(id) = bounded(packet, "requestId", 36) else { return Ok(()) };
        match kind {
            "recovery-capture" => {
                let mut reply = json!({"requestId":id,"documentId":bounded(packet,"documentId",20),"revision":bounded(packet,"revision",20)});
                if Self::targets(app, packet) && !Self::unfinished(app) && !self.suspended() {
                    match app.session.capture_account_draft() {
                        Ok(bytes) => return self.port.publish("recovery-captured", reply, Some(bytes)),
                        Err(error) => reply["message"] = json!(error.to_string()),
                    }
                } else {
                    // Let the host retire this draft request before its correlated reply arrives.
                    self.publish_state(app)?;
                    reply["message"] = json!("The EffectCraft project changed before recovery capture.");
                }
                self.port.publish("recovery-captured", reply, None)
            }
            "session-save" => self.capture_session_save(app, packet, id),
            "session" => self.session(app, ctx, packet, id),
            "dialog-result" => self.dialog_result(app, packet, id),
            "operation" => self.operation(app, ctx, packet, id),
            _ => Ok(()),
        }
    }

    fn next_id(&mut self) -> Result<String, String> {
        self.sequence = self.sequence.checked_add(1).ok_or("EffectCraft exhausted its request identifiers.")?;
        Ok(self.sequence.to_string())
    }

    fn request_dialog(&mut self, mut packet: Value, revision: Option<u64>) -> Result<(), String> {
        let id = self.next_id()?;
        packet["requestId"] = json!(id);
        self.port.publish("dialog", packet, None)?;
        self.dialog = Some((id, revision));
        Ok(())
    }

    fn project_action(&mut self, app: &mut EffectcraftApp, action: AccountProjectRequest) -> Result<(), String> {
        if self.suspended() || self.busy() {
            return Err("Finish the pending EffectCraft project operation first.".into());
        }
        match action {
            AccountProjectRequest::Close => self.port.publish("close-intent", json!({}), None),
            AccountProjectRequest::New => self.request_dialog(json!({"operation":"new","settings":"{}"}), None),
            AccountProjectRequest::Demo => self.request_dialog(json!({"operation":"new","settings":"{\"demo\":true}"}), None),
            AccountProjectRequest::Open(path) => self.request_dialog(json!({"operation":"open","path":path}), None),
            AccountProjectRequest::Save | AccountProjectRequest::SaveAs => {
                if Self::unfinished(app) {
                    return Err("Finish the current EffectCraft text edit before saving.".into());
                }
                if matches!(action, AccountProjectRequest::Save)
                    && let Some(path) = app.session.path.clone()
                {
                    return self.send_save(app, path, "save", json!({}));
                }
                let suggested = app.session.path.clone().unwrap_or_else(|| "Untitled Project.ecproj".to_owned());
                self.request_dialog(json!({"operation":"save","suggested":suggested}), Some(app.session.revision))
            }
        }
    }

    fn dialog_result(&mut self, app: &EffectcraftApp, packet: &JsValue, id: String) -> Result<(), String> {
        if !self.dialog.as_ref().is_some_and(|(expected, _)| expected == &id) {
            return Ok(());
        }
        let Some((_, revision)) = self.dialog.take() else { return Ok(()) };
        match bounded(packet, "outcome", 16).as_deref() {
            Some("failed") => Err(bounded(packet, "message", 250).unwrap_or_else(|| "The EffectCraft dialog failed.".into())),
            Some("destination") if revision.is_some() => {
                if revision != Some(app.session.revision) || Self::unfinished(app) || self.suspended() {
                    return Err("The project changed while choosing its save destination.".into());
                }
                let path = bounded(packet, "path", 2048).ok_or("Invalid EffectCraft save destination.")?;
                self.send_save(app, path, "save", json!({}))
            }
            _ => Ok(()),
        }
    }

    fn capture_session_save(&mut self, app: &EffectcraftApp, packet: &JsValue, id: String) -> Result<(), String> {
        let path = bounded(packet, "path", 2048).ok_or("Invalid EffectCraft save destination.")?;
        let mut reply = json!({"requestId":id,"path":path,"documentId":bounded(packet,"documentId",20),
            "reviewedRevision":bounded(packet,"revision",20)});
        if self.busy() || self.suspended() || Self::unfinished(app) || !Self::targets(app, packet) {
            reply["outcome"] = json!("busy");
            return self.port.publish("session-save-result", reply, None);
        }
        if let Err(error) = self.send_save(app, path, "session-save-result", reply.clone()) {
            reply["outcome"] = json!("failed");
            reply["message"] = json!(error);
            self.port.publish("session-save-result", reply, None)?;
        }
        Ok(())
    }

    fn send_save(&mut self, app: &EffectcraftApp, path: String, kind: &str, mut reply: Value) -> Result<(), String> {
        let (project, bytes) = app.session.prepare_account_save(&path).map_err(|error| error.to_string())?;
        let ticket = self.next_id()?;
        let document = app.session.script_ui.project_epoch.to_string();
        let revision = app.session.revision.to_string();
        reply["outcome"] = json!("captured");
        reply["ticket"] = json!(ticket);
        reply["documentId"] = json!(document);
        reply["revision"] = json!(revision);
        reply["path"] = json!(path);
        self.port.publish(kind, reply, Some(bytes))?;
        self.pending = Some(Save { ticket, document, revision, path, project });
        Ok(())
    }

    fn settle_save(&mut self, app: &mut EffectcraftApp, packet: &JsValue) -> Result<(), String> {
        let matching = self.pending.as_ref().is_some_and(|save| {
            bounded(packet, "ticket", 20).as_deref() == Some(save.ticket.as_str())
                && bounded(packet, "documentId", 20).as_deref() == Some(save.document.as_str())
                && bounded(packet, "revision", 20).as_deref() == Some(save.revision.as_str())
                && bounded(packet, "path", 2048).as_deref() == Some(save.path.as_str())
        });
        let outcome = bounded(packet, "outcome", 16);
        if !matching || !matches!(outcome.as_deref(), Some("written" | "failed" | "unknown")) {
            return Ok(());
        }
        let Some(save) = self.pending.take() else { return Ok(()) };
        let adopted = outcome.as_deref() == Some("written") && app.session.adopt_account_save(save.project).is_ok();
        self.port.publish(
            "save-settled",
            json!({"ticket":save.ticket,"documentId":save.document,"revision":save.revision,
            "path":save.path,"adopted":adopted,"projectState":self.state(app)}),
            None,
        )
    }

    fn session(&mut self, app: &mut EffectcraftApp, ctx: &egui::Context, packet: &JsValue, id: String) -> Result<(), String> {
        let operation = bounded(packet, "operation", 16).ok_or("Invalid EffectCraft session operation.")?;
        let lease = bounded(packet, "lease", 64).filter(|value| !value.is_empty()).ok_or("Invalid EffectCraft session lease.")?;
        let accepted = match operation.as_str() {
            "pause"
                if !self.suspended()
                    && !self.busy()
                    && !Self::unfinished(app)
                    && Self::targets(app, packet)
                    && field(packet, "dirty").as_bool() == Some(app.session.is_dirty())
                    && field(packet, "unfinishedEdit").as_bool() == Some(false) =>
            {
                effectcraft_ui_egui::menus::invoke(app, ctx, "playback.stop", json!({}))?;
                self.lease = Some(lease.clone());
                true
            }
            "release" | "finalize" if self.lease.as_deref() == Some(lease.as_str()) => {
                self.lease = None;
                self.finalized = operation == "finalize";
                true
            }
            _ => false,
        };
        self.port.publish(
            "session-result",
            json!({"requestId":id,"operation":operation,"lease":lease,"ok":accepted,
            "message":if accepted {""} else {"The EffectCraft project changed after close review."}}),
            None,
        )
    }

    fn operation(&self, app: &mut EffectcraftApp, ctx: &egui::Context, packet: &JsValue, id: String) -> Result<(), String> {
        let document = bounded(packet, "documentId", 20).ok_or("Invalid EffectCraft project target.")?;
        let mut reply = json!({"requestId":id,"documentId":document});
        let path = field(packet, "path");
        let path = if path.is_null() { None } else { Some(path.as_string().ok_or("Invalid EffectCraft project binding.")?) };
        let result = if self.suspended() || document != app.session.script_ui.project_epoch.to_string() || path != app.session.path {
            Err("The EffectCraft operation does not match the current project.".into())
        } else {
            let command = bounded(packet, "command", 64).ok_or("Invalid EffectCraft operation.")?;
            let args = bounded(packet, "args", 4096).ok_or("Invalid EffectCraft operation arguments.")?;
            let args: Value = serde_json::from_str(&args).map_err(|_| "Invalid EffectCraft operation arguments.")?;
            let fields = args.as_object().ok_or("EffectCraft operation arguments must be an object.")?;
            match command.as_str() {
                "document.inspect" if fields.is_empty() => Ok(json!({
                    "name":app.session.project_name(),
                    "compositionCount":app.session.project.comps().count(),
                    "activeComposition":app.session.active_comp_id().map(|id| id.0),
                    "mediaCount":project_media(&app.session).len(),
                    "dirty":app.session.is_dirty(),
                })),
                _ if self.busy() || Self::unfinished(app) => Err("Finish the pending EffectCraft edit first.".into()),
                "comp.new" if fields.len() == 3
                    && fields.get("name").and_then(Value::as_str).is_some_and(|name| !name.trim().is_empty() && name.len() <= 128)
                    && ["width", "height"].iter().all(|key| fields.get(*key).and_then(Value::as_u64).is_some_and(|n| (1..=8192).contains(&n))) =>
                {
                    effectcraft_ui_egui::menus::invoke(app, ctx, &command, args)
                }
                "edit.undo" if fields.is_empty() => effectcraft_ui_egui::menus::invoke(app, ctx, &command, args),
                _ => Err("Unsupported EffectCraft project operation.".into()),
            }
        };
        match result {
            Ok(value) => reply["result"] = json!(value.to_string()),
            Err(error) => reply["message"] = json!(error),
        }
        self.port.publish("operation-result", reply, None)
    }
}

fn bounded(packet: &JsValue, field: &str, maximum: usize) -> Option<String> {
    string(packet, field).filter(|text| text.len() <= maximum)
}

fn number(packet: &JsValue, field: &str) -> Option<u64> {
    bounded(packet, field, 20)?.parse().ok()
}

/// Resolve only immutable account links in the iframe's isolated media table.
pub(crate) async fn open(port: &AccountPort, mut session: Session) -> Result<(Session, Value, usize), String> {
    let request = port.start().await?;
    let opened = match request.bytes {
        Some(bytes) => session.open_account_project(request.path.as_deref(), &bytes, request.recover),
        None if request.settings["demo"] == true => session.execute("file.openDemoProject", json!({})).map(|_| ()),
        None => session.execute("file.newProject", json!({})).map(|_| ()),
    };
    if let Err(error) = opened {
        let error = error.to_string();
        port.open_failed(&request.request_id, &error);
        return Err(error);
    }

    let mut missing = 0;
    for path in project_media(&session) {
        let Some((digest, _)) = original_parts(&path) else {
            missing += 1;
            continue;
        };
        match port.read_original(digest).await {
            Ok(bytes) => {
                let bytes: std::sync::Arc<[u8]> = bytes.into();
                files::STORE.put_file(&path, bytes.clone(), false);
                if let Some(importer) = &session.importer {
                    importer.register(&path, &bytes);
                }
            }
            Err(_) => missing += 1,
        }
    }
    let identity = json!({
        "requestId": request.request_id,
        "documentId":session.script_ui.project_epoch.to_string(),
        "revision":session.revision.to_string(),
        "path":session.path,
        "ok":true,
    });
    Ok((session, identity, missing))
}

fn project_media(session: &Session) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    let mut add = |footage: &Footage| {
        if footage.kind != FootageKind::Data {
            paths.extend(std::iter::once(&footage.path).chain(&footage.sequence).filter(|path| !path.is_empty()).cloned());
        }
    };
    for item in session.project.items.values() {
        if let ItemKind::Footage(footage) = &item.kind {
            add(footage);
        }
        if let Some(proxy) = &item.proxy {
            add(&proxy.footage);
        }
    }
    paths
}

const ORIGINAL_PREFIX: &str = "/files/webos-originals/";

fn import_media(port: AccountPort, pending: Rc<Cell<bool>>, picked: Vec<web_sys::File>) {
    if picked.is_empty() {
        return;
    }
    if pending.replace(true) {
        crate::post(|app| app.ui.status = "Finish the current EffectCraft media import first.".into());
        return;
    }
    wasm_bindgen_futures::spawn_local(async move {
        let originals = store_originals(&port, picked).await;
        crate::post(move |app| {
            pending.set(false);
            match originals {
                Ok(paths) if !paths.is_empty() => match app.session.execute("file.import", json!({"paths":paths})) {
                    Ok(result) => {
                        if let Some(errors) = result["errors"].as_array() {
                            let messages = errors.iter().filter_map(Value::as_str).collect::<Vec<_>>();
                            if !messages.is_empty() {
                                app.ui.status = messages.join(" · ");
                            }
                        }
                    }
                    Err(error) => app.ui.status = error.to_string(),
                },
                Err(error) => app.ui.status = error,
                _ => {}
            }
        });
    });
}

async fn store_originals(port: &AccountPort, picked: Vec<web_sys::File>) -> Result<Vec<String>, String> {
    let mut paths = Vec::new();
    let mut errors = Vec::new();
    for file in picked {
        let name = file.name();
        if name.to_ascii_lowercase().ends_with(".ecproj") {
            errors.push("Upload the project to Files, then use Open Project.".into());
            continue;
        }
        if !original_name(&name) {
            errors.push("An original has an invalid filename.".into());
            continue;
        }
        match port.store_original(&file).await {
            Ok((digest, bytes)) => {
                let path = format!("{ORIGINAL_PREFIX}{digest}/{name}");
                files::STORE.put_file(&path, bytes.into(), false);
                paths.push(path);
            }
            Err(error) => errors.push(format!("{name}: {error}")),
        }
    }
    if errors.is_empty() {
        Ok(paths)
    } else if paths.is_empty() {
        Err(errors.join(" · "))
    } else {
        crate::post(move |app| app.ui.status = errors.join(" · "));
        Ok(paths)
    }
}

fn original_name(name: &str) -> bool {
    !name.trim().is_empty() && name.len() <= 255 && !matches!(name, "." | "..")
        && !name.chars().any(|c| matches!(c, '/' | '\\') || c.is_control())
}

fn original_parts(path: &str) -> Option<(&str, &str)> {
    let (digest, name) = path.strip_prefix(ORIGINAL_PREFIX)?.split_once('/')?;
    let digest_ok = digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    (digest_ok && original_name(name)).then_some((digest, name))
}
