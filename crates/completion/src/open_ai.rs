use crate::CompletionProvider;
use crate::LanguageModelCompletionProvider;
use anyhow::{anyhow, Result};
use editor::{Editor, EditorElement, EditorStyle};
use futures::{future::BoxFuture, stream::BoxStream, FutureExt, StreamExt};
use gpui::{AnyView, AppContext, Task, TextStyle, View};
use http::HttpClient;
use language_model::{CloudModel, LanguageModel, LanguageModelRequest, Role};
use open_ai::Model as OpenAiModel;
use open_ai::{stream_completion, Request, RequestMessage};
use settings::Settings;
use std::time::Duration;
use std::{env, sync::Arc};
use strum::IntoEnumIterator;
use theme::ThemeSettings;
use ui::prelude::*;
use util::ResultExt;

pub struct OpenAiCompletionProvider {
    api_key: Option<String>,
    api_url: String,
    model: OpenAiModel,
    http_client: Arc<dyn HttpClient>,
    low_speed_timeout: Option<Duration>,
    settings_version: usize,
    available_models_from_settings: Vec<OpenAiModel>,
}

impl OpenAiCompletionProvider {
    pub fn new(
        model: OpenAiModel,
        api_url: String,
        http_client: Arc<dyn HttpClient>,
        low_speed_timeout: Option<Duration>,
        settings_version: usize,
        available_models_from_settings: Vec<OpenAiModel>,
    ) -> Self {
        Self {
            api_key: None,
            api_url,
            model,
            http_client,
            low_speed_timeout,
            settings_version,
            available_models_from_settings,
        }
    }

    pub fn update(
        &mut self,
        model: OpenAiModel,
        api_url: String,
        low_speed_timeout: Option<Duration>,
        settings_version: usize,
    ) {
        self.model = model;
        self.api_url = api_url;
        self.low_speed_timeout = low_speed_timeout;
        self.settings_version = settings_version;
    }

    fn to_open_ai_request(&self, request: LanguageModelRequest) -> Request {
        let model = match request.model {
            LanguageModel::OpenAi(model) => model,
            _ => self.model.clone(),
        };

        Request {
            model,
            messages: request
                .messages
                .into_iter()
                .map(|msg| match msg.role {
                    Role::User => RequestMessage::User {
                        content: msg.content,
                    },
                    Role::Assistant => RequestMessage::Assistant {
                        content: Some(msg.content),
                        tool_calls: Vec::new(),
                    },
                    Role::System => RequestMessage::System {
                        content: msg.content,
                    },
                })
                .collect(),
            stream: true,
            stop: request.stop,
            temperature: request.temperature,
            tools: Vec::new(),
            tool_choice: None,
        }
    }
}

impl LanguageModelCompletionProvider for OpenAiCompletionProvider {
    fn available_models(&self) -> Vec<LanguageModel> {
        if self.available_models_from_settings.is_empty() {
            let available_models = if matches!(self.model, OpenAiModel::Custom { .. }) {
                vec![self.model.clone()]
            } else {
                OpenAiModel::iter()
                    .filter(|model| !matches!(model, OpenAiModel::Custom { .. }))
                    .collect()
            };
            available_models
                .into_iter()
                .map(LanguageModel::OpenAi)
                .collect()
        } else {
            self.available_models_from_settings
                .iter()
                .cloned()
                .map(LanguageModel::OpenAi)
                .collect()
        }
    }

    fn settings_version(&self) -> usize {
        self.settings_version
    }

    fn is_authenticated(&self) -> bool {
        self.api_key.is_some()
    }

    fn authenticate(&self, cx: &AppContext) -> Task<Result<()>> {
        if self.is_authenticated() {
            Task::ready(Ok(()))
        } else {
            let api_url = self.api_url.clone();
            cx.spawn(|mut cx| async move {
                let api_key = if let Ok(api_key) = env::var("OPENAI_API_KEY") {
                    api_key
                } else {
                    let (_, api_key) = cx
                        .update(|cx| cx.read_credentials(&api_url))?
                        .await?
                        .ok_or_else(|| anyhow!("credentials not found"))?;
                    String::from_utf8(api_key)?
                };
                cx.update_global::<CompletionProvider, _>(|provider, _cx| {
                    provider.update_current_as::<_, Self>(|provider| {
                        provider.api_key = Some(api_key);
                    });
                })
            })
        }
    }

    fn reset_credentials(&self, cx: &AppContext) -> Task<Result<()>> {
        let delete_credentials = cx.delete_credentials(&self.api_url);
        cx.spawn(|mut cx| async move {
            delete_credentials.await.log_err();
            cx.update_global::<CompletionProvider, _>(|provider, _cx| {
                provider.update_current_as::<_, Self>(|provider| {
                    provider.api_key = None;
                });
            })
        })
    }

    fn authentication_prompt(&self, cx: &mut WindowContext) -> AnyView {
        cx.new_view(|cx| AuthenticationPrompt::new(self.api_url.clone(), cx))
            .into()
    }

    fn model(&self) -> LanguageModel {
        LanguageModel::OpenAi(self.model.clone())
    }

    fn count_tokens(
        &self,
        request: LanguageModelRequest,
        cx: &AppContext,
    ) -> BoxFuture<'static, Result<usize>> {
        count_open_ai_tokens(request, cx.background_executor())
    }

    fn stream_completion(
        &self,
        request: LanguageModelRequest,
    ) -> BoxFuture<'static, Result<BoxStream<'static, Result<String>>>> {
        let request = self.to_open_ai_request(request);

        let http_client = self.http_client.clone();
        let api_key = self.api_key.clone();
        let api_url = self.api_url.clone();
        let low_speed_timeout = self.low_speed_timeout;
        async move {
            let api_key = api_key.ok_or_else(|| anyhow!("missing api key"))?;
            let request = stream_completion(
                http_client.as_ref(),
                &api_url,
                &api_key,
                request,
                low_speed_timeout,
            );
            let response = request.await?;
            let stream = response
                .filter_map(|response| async move {
                    match response {
                        Ok(mut response) => Some(Ok(response.choices.pop()?.delta.content?)),
                        Err(error) => Some(Err(error)),
                    }
                })
                .boxed();
            Ok(stream)
        }
        .boxed()
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

pub fn count_open_ai_tokens(
    request: LanguageModelRequest,
    background_executor: &gpui::BackgroundExecutor,
) -> BoxFuture<'static, Result<usize>> {
    background_executor
        .spawn(async move {
            let messages = request
                .messages
                .into_iter()
                .map(|message| tiktoken_rs::ChatCompletionRequestMessage {
                    role: match message.role {
                        Role::User => "user".into(),
                        Role::Assistant => "assistant".into(),
                        Role::System => "system".into(),
                    },
                    content: Some(message.content),
                    name: None,
                    function_call: None,
                })
                .collect::<Vec<_>>();

            match request.model {
                LanguageModel::Anthropic(_)
                | LanguageModel::Cloud(CloudModel::Claude3_5Sonnet)
                | LanguageModel::Cloud(CloudModel::Claude3Opus)
                | LanguageModel::Cloud(CloudModel::Claude3Sonnet)
                | LanguageModel::Cloud(CloudModel::Claude3Haiku)
                | LanguageModel::OpenAi(OpenAiModel::Custom { .. }) => {
                    // Tiktoken doesn't yet support these models, so we manually use the
                    // same tokenizer as GPT-4.
                    tiktoken_rs::num_tokens_from_messages("gpt-4", &messages)
                }
                _ => tiktoken_rs::num_tokens_from_messages(request.model.id(), &messages),
            }
        })
        .boxed()
}

struct AuthenticationPrompt {
    api_key: View<Editor>,
    api_url: String,
}

impl AuthenticationPrompt {
    fn new(api_url: String, cx: &mut WindowContext) -> Self {
        Self {
            api_key: cx.new_view(|cx| {
                let mut editor = Editor::single_line(cx);
                editor.set_placeholder_text(
                    "sk-000000000000000000000000000000000000000000000000",
                    cx,
                );
                editor
            }),
            api_url,
        }
    }

    fn save_api_key(&mut self, _: &menu::Confirm, cx: &mut ViewContext<Self>) {
        let api_key = self.api_key.read(cx).text(cx);
        if api_key.is_empty() {
            return;
        }

        let write_credentials = cx.write_credentials(&self.api_url, "Bearer", api_key.as_bytes());
        cx.spawn(|_, mut cx| async move {
            write_credentials.await?;
            cx.update_global::<CompletionProvider, _>(|provider, _cx| {
                provider.update_current_as::<_, OpenAiCompletionProvider>(|provider| {
                    provider.api_key = Some(api_key);
                });
            })
        })
        .detach_and_log_err(cx);
    }

    fn render_api_key_editor(&self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        let settings = ThemeSettings::get_global(cx);
        let text_style = TextStyle {
            color: cx.theme().colors().text,
            font_family: settings.ui_font.family.clone(),
            font_features: settings.ui_font.features.clone(),
            font_size: rems(0.875).into(),
            font_weight: settings.ui_font.weight,
            line_height: relative(1.3),
            ..Default::default()
        };
        EditorElement::new(
            &self.api_key,
            EditorStyle {
                background: cx.theme().colors().editor_background,
                local_player: cx.theme().players().local(),
                text: text_style,
                ..Default::default()
            },
        )
    }
}

impl Render for AuthenticationPrompt {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        const INSTRUCTIONS: [&str; 6] = [
            "To use the assistant panel or inline assistant, you need to add your OpenAI API key.",
            " - You can create an API key at: platform.openai.com/api-keys",
            " - Make sure your OpenAI account has credits",
            " - Having a subscription for another service like GitHub Copilot won't work.",
            "",
            "Paste your OpenAI API key below and hit enter to use the assistant:",
        ];

        v_flex()
            .p_4()
            .size_full()
            .on_action(cx.listener(Self::save_api_key))
            .children(
                INSTRUCTIONS.map(|instruction| Label::new(instruction).size(LabelSize::Small)),
            )
            .child(
                h_flex()
                    .w_full()
                    .my_2()
                    .px_2()
                    .py_1()
                    .bg(cx.theme().colors().editor_background)
                    .rounded_md()
                    .child(self.render_api_key_editor(cx)),
            )
            .child(
                Label::new(
                    "You can also assign the OPENAI_API_KEY environment variable and restart Zed.",
                )
                .size(LabelSize::Small),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(Label::new("Click on").size(LabelSize::Small))
                    .child(Icon::new(IconName::ZedAssistant).size(IconSize::XSmall))
                    .child(
                        Label::new("in the status bar to close this panel.").size(LabelSize::Small),
                    ),
            )
            .into_any()
    }
}
