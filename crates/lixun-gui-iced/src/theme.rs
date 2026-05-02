use iced::widget::{container, scrollable, text, text_input};
use iced::{Background, Border, Color, Shadow, Vector};

pub const DARK_BG: Color = Color::from_rgba(0.11, 0.11, 0.125, 0.92);
pub const BORDER_COLOR: Color = Color::from_rgba(1.0, 1.0, 1.0, 0.05);
pub const TITLE_COLOR: Color = Color::from_rgba(0.94, 0.94, 0.96, 1.0);
pub const SUBTITLE_COLOR: Color = Color::from_rgba(0.66, 0.66, 0.70, 1.0);
pub const PLACEHOLDER_COLOR: Color = Color::from_rgba(0.48, 0.48, 0.53, 1.0);
pub const CARET_COLOR: Color = Color::from_rgba(0.29, 0.55, 1.0, 1.0);
pub const HIT_HOVER_BG: Color = Color::from_rgba(1.0, 1.0, 1.0, 0.04);
pub const TOP_HIT_BG: Color = Color::from_rgba(1.0, 1.0, 1.0, 0.06);
pub const TOP_HIT_HERO_BG: Color = Color::from_rgba(1.0, 1.0, 1.0, 0.08);

pub fn window_container_style(_theme: &iced::Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(DARK_BG)),
        border: Border {
            radius: 14.0.into(),
            width: 0.5,
            color: BORDER_COLOR,
        },
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.7),
            offset: Vector::new(0.0, 24.0),
            blur_radius: 64.0,
        },
        text_color: Some(TITLE_COLOR),
        snap: false,
    }
}

pub fn search_input_style(_theme: &iced::Theme, status: text_input::Status) -> text_input::Style {
    let base = text_input::Style {
        background: Background::Color(Color::TRANSPARENT),
        border: Border {
            radius: 10.0.into(),
            width: 0.0,
            color: Color::TRANSPARENT,
        },
        icon: SUBTITLE_COLOR,
        placeholder: PLACEHOLDER_COLOR,
        value: TITLE_COLOR,
        selection: Color::from_rgba(0.29, 0.55, 1.0, 0.3),
    };

    match status {
        text_input::Status::Focused { .. } => text_input::Style {
            border: Border {
                radius: 10.0.into(),
                width: 0.0,
                color: Color::TRANSPARENT,
            },
            ..base
        },
        _ => base,
    }
}

pub fn hit_container_style(_theme: &iced::Theme) -> container::Style {
    container::Style {
        background: None,
        border: Border {
            radius: 10.0.into(),
            width: 0.0,
            color: Color::TRANSPARENT,
        },
        shadow: Shadow::default(),
        text_color: Some(TITLE_COLOR),
        snap: false,
    }
}

pub fn top_hit_container_style(_theme: &iced::Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(TOP_HIT_BG)),
        border: Border {
            radius: 10.0.into(),
            width: 0.0,
            color: Color::TRANSPARENT,
        },
        shadow: Shadow::default(),
        text_color: Some(TITLE_COLOR),
        snap: false,
    }
}

pub fn top_hit_hero_container_style(_theme: &iced::Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(TOP_HIT_HERO_BG)),
        border: Border {
            radius: 10.0.into(),
            width: 0.0,
            color: Color::TRANSPARENT,
        },
        shadow: Shadow::default(),
        text_color: Some(TITLE_COLOR),
        snap: false,
    }
}

pub fn scrollable_style(_theme: &iced::Theme, _status: scrollable::Status) -> scrollable::Style {
    let transparent_rail = scrollable::Rail {
        background: None,
        border: Border::default(),
        scroller: scrollable::Scroller {
            background: Background::Color(Color::TRANSPARENT),
            border: Border::default(),
        },
    };

    scrollable::Style {
        container: container::Style::default(),
        vertical_rail: transparent_rail,
        horizontal_rail: transparent_rail,
        gap: None,
        auto_scroll: scrollable::AutoScroll {
            background: Background::Color(Color::TRANSPARENT),
            border: Border::default(),
            shadow: Shadow::default(),
            icon: Color::TRANSPARENT,
        },
    }
}

pub fn title_style(_theme: &iced::Theme) -> text::Style {
    text::Style {
        color: Some(TITLE_COLOR),
    }
}

pub fn subtitle_style(_theme: &iced::Theme) -> text::Style {
    text::Style {
        color: Some(SUBTITLE_COLOR),
    }
}

pub fn calc_expr_style(_theme: &iced::Theme) -> text::Style {
    text::Style {
        color: Some(SUBTITLE_COLOR),
    }
}

pub fn calc_result_style(_theme: &iced::Theme) -> text::Style {
    text::Style {
        color: Some(TITLE_COLOR),
    }
}

pub fn chip_button_style(
    _theme: &iced::Theme,
    _status: iced::widget::button::Status,
    is_active: bool,
) -> iced::widget::button::Style {
    let (bg, text_color) = if is_active {
        (Color::from_rgba(0.29, 0.55, 1.0, 0.25), TITLE_COLOR)
    } else {
        (Color::from_rgba(1.0, 1.0, 1.0, 0.06), SUBTITLE_COLOR)
    };

    iced::widget::button::Style {
        background: Some(Background::Color(bg)),
        text_color,
        border: Border {
            radius: 6.0.into(),
            width: 0.0,
            color: Color::TRANSPARENT,
        },
        shadow: Shadow::default(),
        snap: false,
    }
}
