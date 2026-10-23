use super::*;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use ratatui::widgets::Wrap;

// Backend strings are text, never terminal escapes or clickable control codes.
fn safe(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

impl NativeChat {
    pub(in crate::app) fn header(
        &self,
        codex_running: bool,
        codex_attention: bool,
        switch_hint: &str,
    ) -> Line<'static> {
        let codex = format!(
            " Codex{}{}{} ",
            if codex_running { " ●" } else { "" },
            if codex_attention { " !" } else { "" },
            if self.codex_unread { " •" } else { "" }
        );
        let running = self
            .sessions
            .iter()
            .any(|s| !matches!(s.work, Work::Idle | Work::Unknown));
        let attention = self
            .sessions
            .iter()
            .any(|s| s.work == Work::Unknown || s.write_conflict);
        let unread = self.sessions.iter().any(|s| s.unread);
        let chat = format!(
            " Chat{}{}{} ",
            if running { " ●" } else { "" },
            if unread { " •" } else { "" },
            if attention { " !" } else { "" }
        );
        Line::from(vec![
            if self.active {
                codex.dim()
            } else {
                codex.bold()
            },
            "│".into(),
            if self.active { chat.bold() } else { chat.dim() },
            format!("  {switch_hint} switch · experimental").dim(),
        ])
    }

    pub(super) fn render(&mut self, area: Rect, buffer: &mut Buffer) -> Option<(u16, u16)> {
        Clear.render(area, buffer);
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let Some(session) = self.sessions.get_mut(self.current) else {
            Paragraph::new(safe(&self.notice))
                .wrap(Wrap { trim: false })
                .render(area, buffer);
            return None;
        };
        session.unread = false;
        let mode = if session.persistence == rpc::ChatPersistence::Temporary {
            "Temporary · kept in memory"
        } else {
            "Saved to ChatGPT"
        };
        let model = session
            .content
            .requested_model
            .as_ref()
            .or(self.default_model.as_ref())
            .map_or("backend default", String::as_str);
        let mut lines = vec![
            Line::from(format!(
                "{} · {mode} · {}",
                safe(&session.title),
                safe(model)
            ))
            .bold(),
            Line::default(),
        ];
        for message in &session.messages {
            if !matches!(message.role.as_str(), "user" | "assistant") {
                continue;
            }
            lines.push(
                Line::from(if message.role == "user" {
                    "You"
                } else {
                    "ChatGPT"
                })
                .bold(),
            );
            for part in &message.parts {
                match part {
                    rpc::ChatMessagePart::Text { text } => lines.extend(
                        safe(text)
                            .split('\n')
                            .map(|line| Line::from(line.to_owned())),
                    ),
                    rpc::ChatMessagePart::Image { reference } => lines.push(attachment(reference)),
                    rpc::ChatMessagePart::Unsupported { content_type } => lines.push(
                        Line::from(format!("[Unsupported content: {}]", safe(content_type))).dim(),
                    ),
                }
            }
            for reference in &message.attachments {
                lines.push(attachment(reference));
            }
            for citation in &message.citations {
                lines.push(
                    Line::from(format!(
                        "[{}] {}",
                        safe(&citation.marker),
                        citation
                            .urls
                            .iter()
                            .map(|url| safe(url))
                            .collect::<Vec<_>>()
                            .join(" · ")
                    ))
                    .dim(),
                );
            }
            lines.push(Line::default());
        }
        if let Some(submission) = &session.submission
            && !session
                .turn
                .as_ref()
                .is_some_and(|t| t.acceptance == rpc::ChatAcceptance::Accepted)
        {
            lines.push(
                Line::from("You · acceptance pending").style(crate::style::status_style(
                    crate::style::StatusTone::Attention,
                )),
            );
            lines.extend(
                safe(&submission.text)
                    .split('\n')
                    .map(|line| Line::from(line.to_owned())),
            );
        }
        if let Some(turn) = &session.turn
            && turn.requested_model != turn.resolved_model
            && let Some(resolved) = &turn.resolved_model
        {
            lines.push(
                Line::from(format!("Resolved model: {}", safe(resolved))).style(
                    crate::style::status_style(crate::style::StatusTone::Attention),
                ),
            );
        }
        let composer_height = session
            .composer
            .desired_height(area.width.saturating_sub(2))
            .saturating_add(2)
            .max(3)
            .min((area.height / 3).max(3))
            .min(area.height);
        let selection_height = (session.content.selections.len() as u16)
            .min(4)
            .min(area.height.saturating_sub(composer_height));
        let footer_height = 2.min(
            area.height
                .saturating_sub(composer_height + selection_height),
        );
        let transcript_height = area
            .height
            .saturating_sub(composer_height + selection_height + footer_height);
        // Select the viewport using usize offsets. Paragraph's u16 scroll
        // would otherwise strand the end of a long conversation above view.
        let lines = crate::wrapping::word_wrap_lines(lines, usize::from(area.width));
        let overflow = lines.len().saturating_sub(usize::from(transcript_height));
        session.scroll = session.scroll.min(overflow);
        let offset = overflow.saturating_sub(session.scroll);
        Paragraph::new(
            lines
                .into_iter()
                .skip(offset)
                .take(usize::from(transcript_height))
                .collect::<Vec<_>>(),
        )
        .render(
            Rect::new(area.x, area.y, area.width, transcript_height),
            buffer,
        );
        let mut row = area.y + transcript_height;
        for selection in session
            .content
            .selections
            .iter()
            .take(usize::from(selection_height))
        {
            let progress = session
                .upload
                .as_ref()
                .and_then(|u| u.files.iter().find(|f| f.selection_id == selection.id));
            let state = progress.map_or_else(
                || "selected".into(),
                |p| {
                    format!(
                        "{:?}{}",
                        p.stage,
                        p.percent.map_or(String::new(), |n| format!(" {n}%"))
                    )
                },
            );
            Paragraph::new(format!("[{} · {state}]", safe(&selection.name)))
                .render(Rect::new(area.x, row, area.width, 1), buffer);
            row += 1;
        }
        let composer_area = Rect::new(area.x, row, area.width, composer_height);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(if session.editable() {
                " Message "
            } else {
                " Submission retained · Ctrl-C stop locally "
            });
        let inner = block.inner(composer_area);
        block.render(composer_area, buffer);
        ratatui::widgets::StatefulWidgetRef::render_ref(
            &(&session.composer),
            inner,
            buffer,
            &mut session.editor_state,
        );
        row += composer_height;
        if footer_height > 0 {
            Paragraph::new(safe(if session.notice.is_empty() {
                &self.notice
            } else {
                &session.notice
            }))
            .render(Rect::new(area.x, row, area.width, 1), buffer);
        }
        if footer_height > 1 {
            Paragraph::new("/help · F2 new · F3 chats · F5 refresh · Ctrl-V image")
                .dim()
                .render(Rect::new(area.x, row + 1, area.width, 1), buffer);
        }
        let cursor = session
            .editable()
            .then(|| {
                session
                    .composer
                    .cursor_pos_with_state(inner, session.editor_state)
            })
            .flatten();
        if self.picker.is_some() {
            self.render_picker(area, buffer);
            return None;
        }
        cursor
    }

    fn render_picker(&self, area: Rect, buffer: &mut Buffer) {
        let (title, mut items): (&str, Vec<String>) = match self.picker {
            Some(Picker::Models) => (
                "Model",
                std::iter::once("Backend default".into())
                    .chain(self.models.iter().map(|m| {
                        format!(
                            "{}{}",
                            m.title.as_ref().unwrap_or(&m.id),
                            if m.send_verified {
                                ""
                            } else {
                                " · availability unverified"
                            }
                        )
                    }))
                    .collect(),
            ),
            Some(Picker::History) => (
                "ChatGPT history",
                self.history
                    .iter()
                    .map(|c| {
                        format!(
                            "{}{}",
                            c.title,
                            if c.custom_gpt { " · read only" } else { "" }
                        )
                    })
                    .collect(),
            ),
            Some(Picker::Drafts) => (
                "Saved drafts",
                self.drafts
                    .iter()
                    .map(|d| {
                        if d.preview.is_empty() {
                            "Conversation draft".into()
                        } else {
                            d.preview.clone()
                        }
                    })
                    .collect(),
            ),
            Some(Picker::Sessions) => (
                "Open chats",
                self.sessions
                    .iter()
                    .map(|s| {
                        format!(
                            "{}{}{}",
                            s.title,
                            if s.unread { " •" } else { "" },
                            if s.work == Work::Unknown {
                                " · pending"
                            } else {
                                ""
                            }
                        )
                    })
                    .collect(),
            ),
            Some(Picker::Attachments) => (
                "Attachments · Enter removes",
                self.sessions.get(self.current).map_or_else(Vec::new, |s| {
                    s.content
                        .selections
                        .iter()
                        .map(|f| f.name.clone())
                        .collect()
                }),
            ),
            None => return,
        };
        if self.picker == Some(Picker::History) && self.next_history.is_some()
            || self.picker == Some(Picker::Drafts) && self.next_drafts.is_some()
        {
            items.push("Next page…".into());
        }
        let width = area.width.min(90);
        let height = area
            .height
            .min((items.len() as u16).saturating_add(3).max(4));
        let popup = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        Clear.render(popup, buffer);
        let block = Block::default().borders(Borders::ALL).title(title);
        let inner = block.inner(popup);
        block.render(popup, buffer);
        let start = self
            .selected
            .saturating_sub(usize::from(inner.height.saturating_sub(1)));
        let lines: Vec<_> = items
            .iter()
            .enumerate()
            .skip(start)
            .map(|(i, text)| {
                if i == self.selected {
                    Line::from(format!("> {}", safe(text))).bold()
                } else {
                    Line::from(format!("  {}", safe(text)))
                }
            })
            .collect();
        Paragraph::new(lines).render(inner, buffer);
    }
}

fn attachment(reference: &rpc::ChatAttachmentReference) -> Line<'static> {
    Line::from(format!(
        "[Attachment: {} · {}{}]",
        safe(reference.name.as_deref().unwrap_or("image")),
        safe(reference.mime_type.as_deref().unwrap_or("file")),
        reference
            .size_bytes
            .map_or(String::new(), |size| format!(" · {size} bytes"))
    ))
    .dim()
}
