//! Provider setup is independent of the chat composer and its upcoming redesign.
use crate::i18n::t;
use crate::providers::{self, Provider, ProviderHeader, ProviderModel};
use crate::settings::{settings_button, settings_page_header, settings_primary_button};
use crate::ui::*;
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::{h_flex, v_flex, ActiveTheme as _, Disableable as _, Icon, Sizable as _, StyledExt as _};

struct PairRow {
    key: Entity<InputState>,
    value: Entity<InputState>,
}

struct ProviderForm {
    original_id: Option<String>,
    generation: u64,
    id: Entity<InputState>,
    name: Entity<InputState>,
    base: Entity<InputState>,
    key: Entity<InputState>,
    models: Vec<PairRow>,
    headers: Vec<PairRow>,
    discovering: bool,
    error: Option<String>,
    message: Option<String>,
}

pub(crate) struct ProvidersPage {
    records: Vec<Provider>,
    load_error: Option<String>,
    message: Option<String>,
    form: Option<ProviderForm>,
    generation: u64,
    disconnecting: Option<String>,
    opencode_available: bool,
}

fn input(value: &str, placeholder: &str, masked: bool, window: &mut Window, cx: &mut App) -> Entity<InputState> {
    cx.new(|cx| {
        let mut state = InputState::new(window, cx).placeholder(placeholder.to_owned()).masked(masked);
        state.set_value(value, window, cx);
        state
    })
}

fn pair(key: &str, value: &str, header: bool, window: &mut Window, cx: &mut App) -> PairRow {
    PairRow {
        key: input(key, if header { "Header-Name" } else { "model-id" }, false, window, cx),
        value: input(value, if header { t("Value") } else { t("Display name") }, header, window, cx),
    }
}

impl ProviderForm {
    fn snapshot(&self, cx: &App) -> Provider {
        Provider {
            id: self.id.read(cx).value().trim().to_string(),
            name: self.name.read(cx).value().trim().to_string(),
            base_url: self.base.read(cx).value().trim().trim_end_matches('/').to_string(),
            api_key: self.key.read(cx).value().trim().to_string(),
            models: self.models.iter().filter_map(|row| {
                let id = row.key.read(cx).value().trim().to_string();
                let name = row.value.read(cx).value().trim().to_string();
                if id.is_empty() && name.is_empty() { None } else {
                    Some(ProviderModel { name: if name.is_empty() { id.clone() } else { name }, id })
                }
            }).collect(),
            headers: self.headers.iter().filter_map(|row| {
                let name = row.key.read(cx).value().trim().to_string();
                let value = row.value.read(cx).value().to_string();
                if name.is_empty() && value.is_empty() { None } else { Some(ProviderHeader { name, value }) }
            }).collect(),
        }
    }
}

impl ProvidersPage {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        let (records, load_error) = match providers::load() {
            Ok(records) => (records, None),
            Err(error) => (Vec::new(), Some(error)),
        };
        // CLI probing stays out of render. Configuring providers does not require it installed.
        let weak = cx.entity().downgrade();
        let probe = cx.background_spawn(async {
            wake_core::services::acp::acp_available(wake_core::models::AgentId::Opencode)
        });
        cx.spawn(async move |_, cx| {
            let available = probe.await;
            let _ = weak.update(cx, |this, cx| { this.opencode_available = available; cx.notify(); });
        }).detach();
        Self { records, load_error, message: None, form: None, generation: 0, disconnecting: None, opencode_available: true }
    }

    fn edit(&mut self, record: Option<Provider>, window: &mut Window, cx: &mut Context<Self>) {
        self.generation += 1;
        let original_id = record.as_ref().map(|p| p.id.clone());
        let record = record.unwrap_or_default();
        let models = record.models.iter().map(|m| pair(&m.id, &m.name, false, window, cx)).collect();
        let headers = record.headers.iter().map(|h| pair(&h.name, &h.value, true, window, cx)).collect();
        self.form = Some(ProviderForm {
            original_id, generation: self.generation,
            id: input(&record.id, "myprovider", false, window, cx),
            name: input(&record.name, t("My AI provider"), false, window, cx),
            base: input(&record.base_url, "https://api.example.com/v1", false, window, cx),
            key: input(&record.api_key, t("API key (optional)"), true, window, cx),
            models, headers, discovering: false, error: None, message: None,
        });
        self.message = None;
        self.disconnecting = None;
        if let Some(form) = &self.form { form.name.update(cx, |state, cx| state.focus(window, cx)); }
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.generation += 1; // Late discovery results cannot populate another form.
        self.form = None;
        cx.notify();
    }

    fn add_row(&mut self, header: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.form {
            if form.discovering { return; }
            let row = pair("", "", header, window, cx);
            row.key.update(cx, |state, cx| state.focus(window, cx));
            if header { form.headers.push(row); } else { form.models.push(row); }
            cx.notify();
        }
    }

    fn remove_row(&mut self, header: bool, index: usize, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.form {
            if form.discovering { return; }
            let rows = if header { &mut form.headers } else { &mut form.models };
            if index < rows.len() { rows.remove(index); }
            cx.notify();
        }
    }

    fn save_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.form else { return; };
        if form.discovering { return; }
        let record = form.snapshot(cx);
        if record.models.is_empty() {
            // Connect automatically discovers when the user has not supplied a model list.
            self.discover(true, window, cx);
        } else {
            self.commit(record, cx);
        }
    }

    fn commit(&mut self, record: Provider, cx: &mut Context<Self>) {
        let Some(form) = &self.form else { return; };
        let original = form.original_id.clone();
        let result = providers::validate(&record, true).and_then(|_| {
            let mut records = providers::load()?;
            if records.iter().any(|p| p.id == record.id && Some(&p.id) != original.as_ref()) {
                return Err(t("That provider ID is already in use.").to_string());
            }
            records.retain(|p| Some(&p.id) != original.as_ref());
            records.push(record);
            records.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            providers::save(&records)?;
            Ok(records)
        });
        match result {
            Ok(records) => {
                self.records = records;
                self.form = None;
                self.load_error = None;
                self.message = Some(t("Provider saved. Start a new OpenCode chat and choose its model.").into());
            }
            Err(error) => if let Some(form) = &mut self.form { form.error = Some(error); },
        }
        cx.notify();
    }

    fn discover(&mut self, connect_after: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.form else { return; };
        if form.discovering { return; }
        let request = form.snapshot(cx);
        if let Err(error) = providers::validate(&request, false) {
            if let Some(form) = &mut self.form { form.error = Some(error); }
            cx.notify();
            return;
        }
        let generation = form.generation;
        let form = self.form.as_mut().unwrap();
        form.discovering = true;
        form.error = None;
        form.message = None;
        let (tx, rx) = futures::channel::oneshot::channel();
        let provider = request.clone();
        std::thread::spawn(move || { let _ = tx.send(providers::discover_models(&provider)); });
        cx.spawn_in(window, async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err(t("Model discovery stopped unexpectedly. Please retry.").into()));
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(form) = &mut this.form else { return; };
                if form.generation != generation { return; }
                form.discovering = false;
                // Inputs are disabled during discovery; still guard against programmatic changes.
                if form.snapshot(cx) != request { return; }
                match result {
                    Ok(models) => {
                        let count = models.len();
                        // Preserve manually supplied names and models on refresh.
                        let merged = merge_models(&request.models, models);
                        form.models = merged.iter().map(|m| pair(&m.id, &m.name, false, window, cx)).collect();
                        form.message = Some(crate::tf!("Found {} models. You can edit this list before saving.", count));
                        if connect_after {
                            let mut record = request;
                            record.models = merged;
                            this.commit(record, cx);
                        }
                    }
                    Err(error) => form.error = Some(error),
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }

    fn disconnect(&mut self, id: &str, cx: &mut Context<Self>) {
        let result = providers::load().and_then(|mut records| {
            records.retain(|p| p.id != id);
            providers::save(&records)?;
            Ok(records)
        });
        match result {
            Ok(records) => {
                self.records = records;
                self.disconnecting = None;
                self.message = Some(t("Provider disconnected. Existing chats are unchanged; new chats will no longer load it.").into());
            }
            Err(error) => self.load_error = Some(error),
        }
        cx.notify();
    }

    fn render_list(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let rows = self.records.iter().enumerate().map(|(ix, provider)| {
            let record = provider.clone();
            let id = provider.id.clone();
            let confirming = self.disconnecting.as_ref() == Some(&id);
            v_flex().flex_shrink_0().px(SPACE_LG).py(SPACE_MD).gap(SPACE_SM)
                .when(ix > 0, |row| row.border_t_1().border_color(theme.border))
                .child(h_flex().gap(SPACE_MD).items_center()
                    .child(Icon::empty().path("icons/plug.svg").with_size(px(18.)))
                    .child(v_flex().flex_1().min_w_0().gap(SPACE_XS)
                        .child(div().font_medium().truncate().child(provider.name.clone()))
                        .child(div().text_size(FONT_CAPTION).text_color(theme.muted_foreground).truncate().child(provider.base_url.clone()))
                        .child(div().text_size(FONT_LABEL).text_color(theme.muted_foreground)
                            .child(crate::tf!("{} models · OpenCode", provider.models.len()))))
                    .child(settings_button(Button::new(("provider-edit", ix)).label(t("Edit")), cx)
                        .disabled(confirming)
                        .on_click(cx.listener(move |this, _, window, cx| this.edit(Some(record.clone()), window, cx))))
                    .child(Button::new(("provider-disconnect", ix)).ghost().small().label(t("Disconnect"))
                        .on_click(cx.listener(move |this, _, _, cx| { this.disconnecting = Some(id.clone()); cx.notify(); }))))
                .when(confirming, |row| {
                    let id = provider.id.clone();
                    row.child(div().text_size(FONT_CAPTION).text_color(theme.muted_foreground)
                        .child(t("Remove this provider and its saved credentials from Wake?")))
                        .child(h_flex().gap(SPACE_SM).justify_end()
                            .child(Button::new(("provider-disconnect-cancel", ix)).ghost().small().label(t("Cancel"))
                                .on_click(cx.listener(|this, _, _, cx| { this.disconnecting = None; cx.notify(); })))
                            .child(settings_button(Button::new(("provider-disconnect-confirm", ix)).label(t("Disconnect provider")), cx)
                                .on_click(cx.listener(move |this, _, _, cx| this.disconnect(&id, cx)))))
                })
        }).collect::<Vec<_>>();
        v_flex().flex_1().min_h_0().min_w_0()
            .child(settings_page_header(t("Providers"), t("Connect OpenAI-compatible APIs to OpenCode chats."), cx))
            .child(v_flex().id("providers-list").flex_1().min_h_0().overflow_y_scroll().px(SPACE_XXL).pb(SPACE_XXL).gap(SPACE_LG)
                .when_some(self.load_error.clone(), |this, error| this.child(notice(error, true, cx)))
                .when_some(self.message.clone(), |this, message| this.child(notice(message, false, cx)))
                .when(!self.opencode_available, |this| this.child(notice(t("Install OpenCode to use these providers in chat. You can configure them now.").into(), false, cx)))
                .child(div().font_medium().text_size(FONT_HEADING).child(t("Connected providers")))
                .when(!rows.is_empty(), |this| this.child(v_flex().flex_shrink_0().border_1().border_color(theme.border).rounded(theme.radius_lg).bg(theme.popover).children(rows)))
                .when(self.records.is_empty() && self.load_error.is_none(), |this| this.child(div().text_size(FONT_CAPTION).text_color(theme.muted_foreground)
                    .child(t("No providers connected yet. Add a gateway or a local model server."))))
                .child(h_flex().flex_shrink_0().p(SPACE_LG).gap(SPACE_LG).items_center().border_1().border_color(theme.border).rounded(theme.radius_lg).bg(theme.popover)
                    .child(v_flex().flex_1().min_w_0().gap(SPACE_XS)
                        .child(div().font_medium().child(t("Custom provider")))
                        .child(div().text_size(FONT_CAPTION).text_color(theme.muted_foreground).child(t("Add an OpenAI-compatible provider by base URL."))))
                    .child(settings_button(Button::new("provider-add").label(t("Add provider")).icon(Icon::empty().path("icons/plus.svg")), cx)
                        .disabled(self.load_error.is_some())
                        .on_click(cx.listener(|this, _, window, cx| this.edit(None, window, cx)))))
                .child(div().text_size(FONT_CAPTION).text_color(theme.muted_foreground)
                    .child(t("Keys and headers are stored locally with restricted file permissions, not encrypted. Wake passes them only to its OpenCode process; your OpenCode config files are unchanged."))))
            .into_any_element()
    }

    fn render_pairs(&self, rows: &[PairRow], header: bool, busy: bool, cx: &Context<Self>) -> Div {
        v_flex().gap(SPACE_SM).children(rows.iter().enumerate().map(|(ix, row)| {
            h_flex().gap(SPACE_SM).items_center()
                .child(div().flex_1().min_w_0().child(Input::new(&row.key).disabled(busy)))
                .child(div().flex_1().min_w_0().child(Input::new(&row.value).disabled(busy)))
                .child(Button::new((if header { "provider-header-remove" } else { "provider-model-remove" }, ix))
                    .ghost().small().icon(Icon::empty().path("icons/trash-2.svg").with_size(px(14.)))
                    .tooltip(if header { t("Remove header") } else { t("Remove model") }).disabled(busy)
                    .on_click(cx.listener(move |this, _, _, cx| this.remove_row(header, ix, cx))))
        }))
    }

    fn render_form(&self, form: &ProviderForm, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let busy = form.discovering;
        v_flex().flex_1().min_h_0().min_w_0()
            .child(settings_page_header(if form.original_id.is_some() { t("Edit provider") } else { t("Add provider") }, t("Configure an OpenAI-compatible API. API keys are optional for local servers or header-based authentication."), cx))
            .child(v_flex().id(("provider-form", form.generation)).flex_1().min_h_0().overflow_y_scroll().px(SPACE_XXL).pb(SPACE_LG).gap(SPACE_LG)
                .child(field(t("Provider ID"), &form.id, busy || form.original_id.is_some(), Some(t("Lowercase letters, numbers, hyphens, or underscores. Cannot be changed after connecting.")), cx))
                .child(field(t("Display name"), &form.name, busy, None, cx))
                .child(field(t("Base URL"), &form.base, busy, Some(t("Include the API version path, for example /v1. Use HTTPS, or HTTP for localhost.")), cx))
                .child(field(t("API key"), &form.key, busy, Some(t("Optional. Leave empty for a local server or supply an Authorization header below.")), cx))
                .child(h_flex().items_center().gap(SPACE_SM)
                    .child(div().flex_1().font_medium().child(t("Models")))
                    .child(settings_button(Button::new("provider-discover").label(if busy { t("Detecting models…") } else { t("Detect models") }), cx)
                        .disabled(busy).on_click(cx.listener(|this, _, window, cx| this.discover(false, window, cx)))))
                .child(div().text_size(FONT_CAPTION).text_color(theme.muted_foreground)
                    .child(t("Detection sends your key and headers to Base URL + /models. If unsupported, add model IDs manually. Leave the list empty to detect on Connect.")))
                .when_some(form.message.clone(), |this, message| this.child(notice(message, false, cx)))
                .child(self.render_pairs(&form.models, false, busy, cx))
                .child(Button::new("provider-model-add").ghost().small().label(t("Add model")).icon(Icon::empty().path("icons/plus.svg"))
                    .disabled(busy).on_click(cx.listener(|this, _, window, cx| this.add_row(false, window, cx))))
                .child(div().font_medium().child(t("Headers (optional)")))
                .child(div().text_size(FONT_CAPTION).text_color(theme.muted_foreground).child(t("A custom Authorization header overrides API-key authentication.")))
                .child(self.render_pairs(&form.headers, true, busy, cx))
                .child(Button::new("provider-header-add").ghost().small().label(t("Add header")).icon(Icon::empty().path("icons/plus.svg"))
                    .disabled(busy).on_click(cx.listener(|this, _, window, cx| this.add_row(true, window, cx)))))
            // Keep validation and actions visible even after discovering a long model list.
            .child(v_flex().flex_shrink_0().px(SPACE_XXL).py(SPACE_MD).gap(SPACE_SM).border_t_1().border_color(theme.border)
                .when_some(form.error.clone(), |this, error| this.child(notice(error, true, cx)))
                .child(h_flex().gap(SPACE_SM).justify_end()
                    .child(Button::new("provider-cancel").ghost().label(t("Cancel"))
                        .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))))
                    .child(settings_primary_button(Button::new("provider-save").label(if busy { t("Detecting models…") } else if form.original_id.is_some() { t("Save changes") } else { t("Connect") }), cx)
                        .disabled(busy).on_click(cx.listener(|this, _, window, cx| this.save_form(window, cx))))))
            .into_any_element()
    }
}

fn field(label: &'static str, state: &Entity<InputState>, disabled: bool, hint: Option<&'static str>, cx: &App) -> Div {
    v_flex().flex_shrink_0().gap(SPACE_SM)
        .child(div().text_size(FONT_CAPTION).text_color(cx.theme().muted_foreground).child(label))
        .child(Input::new(state).disabled(disabled))
        .when_some(hint, |this, hint| this.child(div().text_size(FONT_CAPTION).text_color(cx.theme().muted_foreground).child(hint)))
}

fn notice(message: String, error: bool, cx: &App) -> Div {
    div().flex_shrink_0().text_size(FONT_CAPTION)
        .text_color(if error { cx.theme().danger } else { cx.theme().muted_foreground })
        .child(message)
}

fn merge_models(existing: &[ProviderModel], discovered: Vec<ProviderModel>) -> Vec<ProviderModel> {
    let mut merged = existing.to_vec();
    for model in discovered {
        if !merged.iter().any(|m| m.id == model.id) { merged.push(model); }
    }
    merged
}

impl Render for ProvidersPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().size_full().min_w_0().bg(cx.theme().background).child(match &self.form {
            Some(form) => self.render_form(form, cx),
            None => self.render_list(cx),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{merge_models, ProviderModel};
    #[test]
    fn discovery_merge_preserves_custom_names_and_manual_models() {
        let existing = vec![ProviderModel { id: "a".into(), name: "Custom".into() }];
        let result = merge_models(&existing, vec![
            ProviderModel { id: "a".into(), name: "Detected".into() },
            ProviderModel { id: "b".into(), name: "New".into() },
        ]);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].name, "Custom");
        assert_eq!(result[1].id, "b");
    }
}
