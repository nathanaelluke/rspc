use anyhow::Result;
use ratatui::{
    Frame,
    layout::Rect,
    macros::constraint,
    prelude::{Constraint, Layout},
    style::Style,
    symbols::border,
    widgets::{Block, Borders, Clear},
};

use super::{BUTTON_GROUP_SYMBOLS, Modal};
use crate::{
    config::keys::CommonAction,
    ctx::Ctx,
    shared::{
        id::{self, Id},
        keys::ActionEvent,
        mouse_event::{MouseEvent, MouseEventKind},
    },
    ui::{
        input::{BufferId, InputResultEvent},
        widgets::{
            button::{Button, ButtonGroup, ButtonGroupState},
            input::Input,
        },
    },
};

pub struct InputModal<'a, C: FnOnce(&Ctx, &str) -> Result<()> + 'a> {
    id: Id,
    button_group_state: ButtonGroupState,
    button_group: ButtonGroup<'a>,
    input_area: Rect,
    callback: Option<C>,
    title: &'a str,
    input_label: &'a str,
    input_buffer_id: BufferId,
}

impl<Callback: FnOnce(&Ctx, &str) -> Result<()>> std::fmt::Debug for InputModal<'_, Callback> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "InputModal(message = {}, button_group = {:?}, button_group_state = {:?})",
            self.input_label, self.button_group, self.button_group_state,
        )
    }
}

impl<'a, C: FnOnce(&Ctx, &str) -> Result<()> + 'a> InputModal<'a, C> {
    pub fn new(ctx: &Ctx) -> Self {
        Self::with_value(ctx, "")
    }

    pub fn with_value(ctx: &Ctx, value: &str) -> Self {
        let mut button_group_state = ButtonGroupState::default();
        let buttons = vec![
            Button::default().label("Save"),
            Button::default().label("Cancel"),
        ];
        button_group_state.set_button_count(buttons.len());

        let button_group = ButtonGroup::default()
            .buttons(buttons)
            .inactive_style(ctx.config.as_text_style())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_set(BUTTON_GROUP_SYMBOLS)
                    .border_style(ctx.config.as_border_style()),
            );

        let input_buffer_id = BufferId::new();
        ctx.input.create_buffer(input_buffer_id, Some(value));
        ctx.input.insert_mode(input_buffer_id);

        Self {
            id: id::new(),
            button_group_state,
            button_group,
            input_area: Rect::default(),
            callback: None,
            input_label: "",
            title: "",
            input_buffer_id,
        }
    }

    pub fn confirm_label(mut self, label: &'a str) -> Self {
        let buttons = vec![
            Button::default().label(label),
            Button::default().label("Cancel"),
        ];
        self.button_group = self.button_group.buttons(buttons);
        self
    }

    pub fn on_confirm(mut self, callback: C) -> Self {
        self.callback = Some(callback);
        self
    }

    pub fn input_label(mut self, label: &'a str) -> Self {
        self.input_label = label;
        self
    }

    pub fn title(mut self, message: &'a str) -> Self {
        self.title = message;
        self
    }

    fn close(&mut self, ctx: &Ctx, save: bool) -> Result<()> {
        if save && let Some(callback) = self.callback.take() {
            (callback)(ctx, &ctx.input.value(self.input_buffer_id))?;
        }
        ctx.input.destroy_buffer(self.input_buffer_id);
        self.hide(ctx)
    }
}

impl<'a, C: FnOnce(&Ctx, &str) -> Result<()> + 'a> Modal for InputModal<'a, C> {
    fn id(&self) -> Id {
        self.id
    }

    fn owns_input_buffer(&self, id: BufferId) -> bool {
        id == self.input_buffer_id
    }

    fn render(&mut self, frame: &mut Frame, ctx: &mut Ctx) -> Result<()> {
        let block = Block::default()
            .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
            .padding(ratatui::widgets::Padding::horizontal(1))
            .border_set(border::PLAIN)
            .border_style(ctx.config.as_border_style())
            .title_alignment(ratatui::prelude::Alignment::Center)
            .title(self.title);

        let popup_area = frame.area().centered(constraint!(==50), constraint!(==7));
        frame.render_widget(Clear, popup_area);
        if let Some(bg_color) = ctx.config.theme.modal_background_color {
            frame.render_widget(
                Block::default().style(Style::default().bg(bg_color)),
                popup_area,
            );
        }
        let [body_area, buttons_area] =
            *Layout::vertical([Constraint::Length(4), Constraint::Max(3)]).split(popup_area)
        else {
            return Ok(());
        };

        let input = Input::builder()
            .ctx(ctx)
            .buffer_id(self.input_buffer_id)
            .label(self.input_label)
            .label_style(ctx.config.as_text_style())
            .focused(ctx.input.is_active(self.input_buffer_id))
            .focused_style(ctx.config.theme.highlight_border_style)
            .unfocused_style(ctx.config.as_border_style())
            .build();

        self.button_group
            .set_active_style(if ctx.input.is_active(self.input_buffer_id) {
                Style::default().reversed()
            } else {
                ctx.config.theme.current_item_style
            });

        self.input_area = body_area;

        frame.render_widget(input, block.inner(body_area));
        frame.render_widget(block, body_area);
        frame.render_stateful_widget(
            &mut self.button_group,
            buttons_area,
            &mut self.button_group_state,
        );
        Ok(())
    }

    fn handle_insert_mode(&mut self, kind: InputResultEvent, ctx: &mut Ctx) -> Result<()> {
        match kind {
            InputResultEvent::Push => {}
            InputResultEvent::Pop => {}
            InputResultEvent::Confirm => {
                self.close(ctx, self.button_group_state.selected == 0)?;
            }
            InputResultEvent::NoChange => {}
            InputResultEvent::Cancel => {}
        }
        ctx.render()?;
        Ok(())
    }

    fn handle_key(&mut self, key: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        if let Some(action) = key.claim_common() {
            match action {
                CommonAction::Down => {
                    self.button_group_state.next();

                    ctx.render()?;
                }
                CommonAction::Up => {
                    self.button_group_state.next();

                    ctx.render()?;
                }
                CommonAction::Close => {
                    self.close(ctx, false)?;
                }
                CommonAction::Confirm => {
                    self.close(ctx, self.button_group_state.selected == 0)?;
                }
                CommonAction::FocusInput => {
                    ctx.input.insert_mode(self.input_buffer_id);
                    ctx.render()?;
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Result<()> {
        match event.kind {
            MouseEventKind::LeftClick => {
                if let Some(idx) = self.button_group.get_button_idx_at(event.into()) {
                    self.button_group_state.select(idx);
                    ctx.input.normal_mode();
                    ctx.render()?;
                }
            }
            MouseEventKind::DoubleClick => {
                match self.button_group.get_button_idx_at(event.into()) {
                    Some(index) => {
                        self.close(ctx, index == 0)?;
                    }
                    None => {
                        if self.input_area.contains(event.into()) {
                            ctx.input.insert_mode(self.input_buffer_id);
                            ctx.render()?;
                        }
                    }
                }
            }
            MouseEventKind::MiddleClick => {}
            MouseEventKind::RightClick => {}
            MouseEventKind::ScrollUp => {
                if self.button_group.get_button_idx_at(event.into()).is_some() {
                    ctx.input.normal_mode();
                    self.button_group_state.prev();
                    ctx.render()?;
                }
            }
            MouseEventKind::ScrollDown => {
                if self.button_group.get_button_idx_at(event.into()).is_some() {
                    ctx.input.normal_mode();
                    self.button_group_state.next();
                    ctx.render()?;
                }
            }
            MouseEventKind::Drag {
                drag_start_position: _,
            } => {}
        }
        Ok(())
    }
}
