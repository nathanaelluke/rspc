use anyhow::{Context, Result};
use ratatui::{Frame, layout::Position, prelude::Rect, widgets::Widget};
use unicode_width::UnicodeWidthStr;

use super::Pane;
use crate::{
    config::tabs::TabName,
    ctx::Ctx,
    shared::{
        events::AppEvent,
        keys::ActionEvent,
        mouse_event::{MouseEvent, MouseEventKind},
    },
    ui::{UiAppEvent, UiEvent, widgets::tabs::Tabs},
};

#[derive(Debug)]
pub struct TabsPane<'a> {
    area: Rect,
    active_tab: TabName,
    tabs: Tabs<'a>,
}

impl TabsPane<'_> {
    pub fn new(ctx: &Ctx) -> Result<Self> {
        let active_tab = Self::init_active_tab(ctx)?;
        let tab_names = Self::init_tab_names(ctx);
        let tabs = Self::init_tabs(tab_names, ctx);

        Ok(Self {
            area: Rect::default(),
            active_tab,
            tabs,
        })
    }

    pub fn get_tab_idx_at(&self, position: Position) -> Option<usize> {
        self.tabs
            .areas
            .iter()
            .enumerate()
            .find(|(_, area)| area.contains(position))
            .map(|v| v.0)
    }

    fn init_active_tab(ctx: &Ctx) -> Result<TabName> {
        Ok(ctx
            .config
            .tabs
            .names
            .first()
            .context("Expected at least one tab")?
            .clone())
    }

    fn init_tab_names(ctx: &Ctx) -> Vec<String> {
        let style = ctx.config.theme.tab_bar;
        ctx.config
            .tabs
            .names
            .iter()
            .map(|name| {
                let extra = usize::from(style.min_width).saturating_sub(name.0.width());
                let extra = if style.fill {
                    extra.div_ceil(2) * 2
                } else {
                    extra
                };
                let left = " ".repeat(usize::from(style.padding) + extra / 2);
                let right = " ".repeat(usize::from(style.padding) + extra.div_ceil(2));
                format!("{left}{name}{right}")
            })
            .collect()
    }

    fn init_tabs<'a>(tab_names: Vec<String>, ctx: &Ctx) -> Tabs<'a> {
        Tabs::new(tab_names)
            .divider("")
            .fill(ctx.config.theme.tab_bar.fill)
            .style(ctx.config.theme.tab_bar.inactive_style)
            .alignment(ctx.config.theme.tab_bar.alignment)
            .highlight_style(ctx.config.theme.tab_bar.active_style)
    }
}

impl Pane for TabsPane<'_> {
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> anyhow::Result<()> {
        self.area = area;
        if self.area.height > 0 {
            let Some(selected_tab) = ctx
                .config
                .tabs
                .names
                .iter()
                .enumerate()
                .find(|(_, t)| **t == self.active_tab)
                .map(|(idx, _)| idx)
            else {
                return Ok(());
            };

            self.tabs.select(selected_tab);
            self.tabs.render(area, frame.buffer_mut());
        }
        Ok(())
    }

    fn before_show(&mut self, _ctx: &Ctx) -> Result<()> {
        Ok(())
    }

    fn on_event(&mut self, event: &mut UiEvent, _is_visible: bool, ctx: &Ctx) -> Result<()> {
        match event {
            UiEvent::TabChanged(tab) => {
                self.active_tab = tab.clone();
                ctx.render()?;
            }
            UiEvent::ConfigChanged => {
                let new_active_tab = ctx
                    .config
                    .tabs
                    .names
                    .iter()
                    .find(|tab| tab == &&self.active_tab)
                    .or(ctx.config.tabs.names.first())
                    .context("Expected at least one tab")
                    .cloned()?;

                let tab_names = Self::init_tab_names(ctx);
                self.tabs = Self::init_tabs(tab_names, ctx);

                self.active_tab = new_active_tab;
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        if !self.area.contains(event.into()) {
            return Ok(());
        }

        if !matches!(
            event.kind,
            MouseEventKind::LeftClick | MouseEventKind::DoubleClick
        ) {
            return Ok(());
        }

        let Some(tab_name) = self
            .get_tab_idx_at(event.into())
            .and_then(|idx| ctx.config.tabs.names.get(idx))
        else {
            return Ok(());
        };

        if &self.active_tab == tab_name {
            return Ok(());
        }

        ctx.app_event_sender
            .send(AppEvent::UiEvent(UiAppEvent::ChangeTab(tab_name.clone())))?;

        Ok(())
    }

    fn handle_action(&mut self, _event: &mut ActionEvent, _ctx: &mut Ctx) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ratatui::{Terminal, backend::TestBackend, layout::Alignment};
    use rstest::rstest;

    use super::*;
    use crate::tests::fixtures::ctx;

    #[rstest]
    fn compact_tabs_keep_uniform_padding_and_mouse_targets(mut ctx: Ctx) {
        let config = Arc::make_mut(&mut ctx.config);
        config.tabs.names = vec!["Queue".into(), "Album Artists".into(), "音楽".into()];
        config.theme.tab_bar.padding = 1;
        config.theme.tab_bar.min_width = 0;
        config.theme.tab_bar.alignment = Alignment::Left;
        let mut pane = TabsPane::new(&ctx).expect("tabs");
        let mut terminal = Terminal::new(TestBackend::new(80, 1)).expect("test terminal");
        terminal
            .draw(|frame| pane.render(frame, frame.area(), &ctx).expect("render tabs"))
            .expect("draw terminal");
        let areas = &pane.tabs.areas;
        assert_eq!(
            areas
                .iter()
                .map(|area| (area.x, area.width))
                .collect::<Vec<_>>(),
            [(0, 7), (7, 15), (22, 6)]
        );
        assert_eq!(pane.get_tab_idx_at(Position::new(7, 0)), Some(1));
        assert_eq!(pane.get_tab_idx_at(Position::new(22, 0)), Some(2));
    }

    #[rstest]
    fn existing_themes_keep_the_rmpc_tab_spacing(mut ctx: Ctx) {
        Arc::make_mut(&mut ctx.config).tabs.names = vec!["Queue".into(), "Directories".into()];
        assert_eq!(
            TabsPane::init_tab_names(&ctx),
            ["    Queue    ", "  Directories  "]
        );
        assert_eq!(
            TabsPane::init_tabs(TabsPane::init_tab_names(&ctx), &ctx).alignment,
            Alignment::Center
        );
    }
}
