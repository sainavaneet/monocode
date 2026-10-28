use super::*;
use monocode_core::mono::files;

impl Monos {
    pub(crate) fn reset_defaults(
        &mut self,
        id: &str,
        cx: &mut Context<Self>,
    ) -> gpui::Task<Result<(), String>> {
        let root = self.root.clone();
        let id = id.to_owned();
        let target = id.clone();
        let work = cx.background_executor().spawn(async move {
            let current = files::load(&root, &target, None)?;
            files::save(
                &root,
                &target,
                None,
                "SOUL.md",
                mono::DEFAULT_SOUL,
                Some(&current.soul_hash),
            )?;
            files::load(&root, &target, None)
        });
        cx.spawn(async move |this, cx| {
            let files = work.await?;
            this.update(cx, |this, cx| {
                let mono = this.roster.get_mut(&id).ok_or("Mono no longer exists")?;
                mono.name.clear();
                let key = format!("{id}:SOUL.md");
                this.fields.remove(&format!("file:{key}"));
                this.file_versions.remove(&key);
                this.file_hashes.remove(&key);
                this.file_conflicts.remove(&key);
                if this.active.as_deref() == Some(&id) {
                    this.files = Some(files);
                }
                this.configure_chat(&id, cx);
                this.save(cx);
                Ok(())
            })
            .map_err(|e| e.to_string())?
        })
    }
    fn file_editor(
        &mut self,
        path: &str,
        text: String,
        hash: String,
        cx: &mut Context<Self>,
    ) -> Entity<Input> {
        let key = format!("{}:{path}", self.active.as_deref().unwrap_or_default());
        let field_key = format!("file:{key}");
        // A version this editor already opened or saved is not news: a
        // reload that started before a save and landed after it must not
        // revert the editor to the text it had before.
        let seen = self.remember_hash(&key, &hash);
        if let Some((old, old_hash)) = self.file_versions.get(&key)
            && old_hash != &hash
            && !seen
            && !self.saving_files.contains(&key)
            && let Some(field) = self
                .fields
                .get(&field_key)
                .filter(|f| f.read(cx).content.as_str() == old)
        {
            field.update(cx, |f, cx| f.set_text(text.clone(), cx));
            self.file_versions
                .insert(key.clone(), (text.clone(), hash.clone()));
        }
        self.file_versions
            .entry(key)
            .or_insert((text.clone(), hash));
        self.field(&field_key, "Markdown", text, true, cx)
    }

    /// Records `hash` as a version the editor for `key` knows; true when it
    /// already did.
    fn remember_hash(&mut self, key: &str, hash: &str) -> bool {
        const REMEMBERED: usize = 8;
        let known = self.file_hashes.entry(key.to_owned()).or_default();
        if known.iter().any(|h| h == hash) {
            return true;
        }
        known.push(hash.to_owned());
        if known.len() > REMEMBERED {
            known.remove(0);
        }
        false
    }
    pub(super) fn soul_page(&mut self, mono: &Mono, cx: &mut Context<Self>) -> AnyElement {
        let body = if let Some(files) = self.files.clone() {
            let editor = self.file_editor(
                "SOUL.md",
                files.soul.unwrap_or_default(),
                files.soul_hash,
                cx,
            );
            editor.update(cx, |v, cx| {
                v.set_placeholder("Who it is, and what it should always keep in mind", cx)
            });
            let focus = editor.focus_handle(cx);
            div()
                .id("mono-soul-editor")
                .debug_selector(|| "mono-soul-editor".into())
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .px_3()
                .py_3()
                .font_family(crate::markdown::MONO)
                .text_size(px(13.))
                .line_height(px(20.))
                .text_color(content(0.85))
                .on_mouse_down(MouseButton::Left, move |_, w, cx| w.focus(&focus, cx))
                .child(editor)
                .into_any_element()
        } else {
            div()
                .flex_1()
                .p_6()
                .text_center()
                .text_size(px(12.))
                .text_color(content(0.45))
                .child("Loading…")
                .into_any_element()
        };
        let _ = mono;
        let conflict = self.conflict_bar("SOUL.md", cx);
        div()
            .id("mono-soul-page")
            .debug_selector(|| "mono-soul-page".into())
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(Self::settings_page_header("Soul"))
            .child(body)
            .children(conflict)
            .into_any_element()
    }
    pub(super) fn settings_page_header(title: &'static str) -> gpui::Div {
        div()
            .h(px(40.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_1()
            .pl_4()
            .pr(px(44.))
            .border_b_1()
            .border_color(content(0.04))
            .text_size(px(13.))
            .font_weight(gpui::FontWeight::MEDIUM)
            .child(div().flex_1().min_w_0().child(title))
    }
    pub(super) fn bind_settings_blurs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fields = self
            .fields
            .iter()
            .filter(|(key, _)| key.starts_with("file:"))
            .map(|(_, field)| field.clone())
            .collect::<Vec<_>>();
        for field in fields {
            if self.field_blurs.insert(field.entity_id()) {
                self.observations.push(cx.on_blur(
                    &field.focus_handle(cx),
                    window,
                    |this, _, cx| this.save_open_file(cx),
                ));
            }
        }
    }
    pub(super) fn save_open_file(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.active.clone() else {
            return;
        };
        let prefix = format!("{id}:");
        for (key, (old, hash)) in self.file_versions.clone() {
            let Some(path) = key.strip_prefix(&prefix).map(str::to_owned) else {
                continue;
            };
            let Some(field) = self.fields.get(&format!("file:{key}")) else {
                continue;
            };
            let text = field.read(cx).content.to_string();
            if text == old || hash.is_empty() || !self.saving_files.insert(key.clone()) {
                continue;
            }
            let root = self.root.clone();
            let id = id.clone();
            let target = id.clone();
            let content = text.clone();
            let work = cx.background_executor().spawn(async move {
                match files::save(&root, &target, None, &path, &content, Some(&hash)) {
                    Ok(hash) => Ok(hash),
                    // A conflict: say what the disk holds now, so the editor
                    // can tell hash drift from a real change.
                    Err(e) if e == files::CONFLICT => {
                        match files::read(&root, &target, None, &path) {
                            Ok(disk) => Err((disk.text.unwrap_or_default(), disk.hash)),
                            Err(e) => Err((e, String::new())),
                        }
                    }
                    Err(e) => Err((e, String::new())),
                }
                .map_err(|(text, hash)| (path, text, hash))
            });
            cx.spawn(async move |this, cx| {
                let result = work.await;
                let _ = this.update(cx, |this, cx| {
                    this.saving_files.remove(&key);
                    match result {
                        Ok(hash) => {
                            this.file_conflicts.remove(&key);
                            this.adopt_saved(&key, &text, &hash);
                            this.reload_files(&id, cx);
                        }
                        Err((_, error, disk_hash)) if disk_hash.is_empty() => {
                            this.error = Some(format!(
                                "Could not save: {error}. Your edits are still in the editor."
                            ))
                        }
                        // The disk still holds the text the editor opened:
                        // only the hash drifted, so adopt it and save again.
                        Err((_, disk, disk_hash)) if disk == old => {
                            this.file_versions.insert(key, (old, disk_hash));
                            this.save_open_file(cx);
                        }
                        Err((path, disk, disk_hash)) => {
                            this.file_conflicts.insert(key, (disk, disk_hash));
                            this.error = Some(format!(
                                "{path} changed on disk while you were editing (your Mono or another window wrote it). Your edits are still in the editor: choose Keep mine or Take theirs below the editor."
                            ));
                        }
                    }
                    cx.notify();
                });
            })
            .detach();
        }
    }

    /// The editor's saved version, and the loaded files with it, so a
    /// render before the reload finishes does not revert the editor to the
    /// stale copy (which used to leave the editor with a dead hash and every
    /// later save in conflict).
    fn adopt_saved(&mut self, key: &str, text: &str, hash: &str) {
        self.remember_hash(key, hash);
        self.file_versions
            .insert(key.to_owned(), (text.to_owned(), hash.to_owned()));
        if key.ends_with(":SOUL.md")
            && let Some(files) = &mut self.files
        {
            files.soul = Some(text.to_owned());
            files.soul_hash = hash.to_owned();
        }
    }

    /// Resolves a save conflict: keep the editor's text (overwrite the disk)
    /// or take the disk's text (discard the edits).
    pub(super) fn resolve_file_conflict(
        &mut self,
        path: &str,
        keep_mine: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.active.clone() else {
            return;
        };
        let key = format!("{id}:{path}");
        let Some((disk, disk_hash)) = self.file_conflicts.remove(&key) else {
            return;
        };
        self.error = None;
        if !keep_mine {
            if let Some(field) = self.fields.get(&format!("file:{key}")) {
                field.update(cx, |f, cx| f.set_text(disk.clone(), cx));
            }
            self.adopt_saved(&key, &disk, &disk_hash);
            self.reload_files(&id, cx);
            cx.notify();
            return;
        }
        // Mine: the disk's current hash is what the save expects now.
        if let Some((_, hash)) = self.file_versions.get_mut(&key) {
            *hash = disk_hash;
        }
        self.save_open_file(cx);
    }

    /// The bar under a file editor whose save conflicted.
    fn conflict_bar(&self, path: &'static str, cx: &mut Context<Self>) -> Option<AnyElement> {
        let key = format!("{}:{path}", self.active.as_deref().unwrap_or_default());
        if !self.file_conflicts.contains_key(&key) {
            return None;
        }
        let button = |id: &'static str, label: &'static str, keep: bool| {
            div()
                .id(id)
                .debug_selector(move || id.into())
                .role(Role::Button)
                .aria_label(label)
                .cursor_pointer()
                .px_2()
                .py_1()
                .rounded_md()
                .bg(content(0.08))
                .hover(|s| s.bg(content(0.14)))
                .text_size(px(12.))
                .child(label)
                .on_click(
                    cx.listener(move |this, _, _, cx| this.resolve_file_conflict(path, keep, cx)),
                )
        };
        Some(
            div()
                .id("mono-file-conflict")
                .debug_selector(|| "mono-file-conflict".into())
                .flex_shrink_0()
                .px_3()
                .py_2()
                .border_t_1()
                .border_color(content(0.06))
                .flex()
                .items_center()
                .gap_2()
                .text_size(px(12.))
                .text_color(content(0.7))
                .child(div().flex_1().child(format!("{path} changed on disk.")))
                .child(button("mono-file-keep-mine", "Keep mine", true))
                .child(button("mono-file-take-theirs", "Take theirs", false))
                .into_any_element(),
        )
    }

    pub(super) fn file_work(
        &mut self,
        id: String,
        work: impl FnOnce() -> Result<(), String> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let work = cx.background_executor().spawn(async move { work() });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.reload_files(&id, cx);
                    }
                    Err(e) => this.error = Some(e),
                }
                this.changed(cx);
            });
        })
        .detach();
    }
    pub(super) fn reset_conversation(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prompt = window.prompt(
            gpui::PromptLevel::Warning,
            "Reset conversation?",
            Some("The chat will be cleared. Its soul, memory and habits stay."),
            &["Cancel", "Reset"],
            cx,
        );
        let id = id.to_string();
        cx.spawn(async move |this, cx| {
            if prompt.await.ok() != Some(1) {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                if let Some(view) = this.chats.remove(&id) {
                    view.update(cx, |v, cx| v.mono_stop(cx));
                }
                if let Some(mono) = this.roster.get_mut(&id) {
                    mono.session_id = None;
                    mono.unavailable_session = None;
                    mono.seen_rows = 0;
                    mono.outbox.clear();
                }
                this.save(cx);
                this.ensure_chat(&id, cx);
            });
        })
        .detach();
    }
    pub(super) fn delete_mono(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = window.prompt(
            gpui::PromptLevel::Warning,
            "Delete mono?",
            Some("Its scheduled habits stop. Sessions and notes it created stay saved."),
            &["Cancel", "Delete"],
            cx,
        );
        let id = id.to_string();
        cx.spawn(async move |this, cx| {
            if prompt.await.ok() != Some(1) {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                if let Some(view) = this.chats.remove(&id) {
                    view.update(cx, |v, cx| v.mono_stop(cx));
                }
                for run in this.running.values().filter(|r| r.mono == id) {
                    run.view.update(cx, |v, cx| v.mono_stop(cx));
                }
                this.roster.monos.retain(|m| m.id != id);
                if this.active.as_ref() == Some(&id) {
                    this.active = None;
                }
                this.save(cx);
            });
        })
        .detach();
    }
}
