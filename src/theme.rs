//! 단호박 할로윈 테마 7종 (디자인샘플 01~07). 상단 바에서 즉시 전환할 수 있다.

use eframe::egui::{
    self, Color32, CornerRadius, Frame, Margin, Mesh, Painter, Pos2, Rect, RichText, Stroke, Ui, epaint::Shadow,
};

#[derive(Clone, Copy)]
pub struct Palette {
    pub name: &'static str,
    pub tag: &'static str,
    pub light: bool,
    pub fog: bool,
    pub bg: Color32,
    pub panel: Color32,
    pub panel2: Color32,
    pub line: Color32,
    pub text: Color32,
    pub sub: Color32,
    pub accent: Color32,
    pub accent_dark: Color32,
    pub hi: Color32,
    pub on_accent: Color32,
}

const fn c(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

pub const THEMES: [Palette; 7] = [
    Palette {
        name: "01 클래식 할로윈 나이트",
        tag: "CLASSIC · NIGHT",
        light: false,
        fog: false,
        bg: c(0x14, 0x09, 0x1f),
        panel: c(0x1f, 0x12, 0x30),
        panel2: c(0x2a, 0x1a, 0x40),
        line: c(0x43, 0x28, 0x5f),
        text: c(0xf6, 0xec, 0xff),
        sub: c(0xb9, 0xa5, 0xd1),
        accent: c(0xff, 0x7a, 0x1a),
        accent_dark: c(0xd9, 0x48, 0x0f),
        hi: c(0xff, 0xb3, 0x47),
        on_accent: c(0x1a, 0x0b, 0x00),
    },
    Palette {
        name: "02 잭오랜턴 글로우",
        tag: "JACK · GLOW",
        light: false,
        fog: false,
        bg: c(0x07, 0x05, 0x07),
        panel: c(0x12, 0x0d, 0x0a),
        panel2: c(0x1d, 0x14, 0x0e),
        line: c(0x5c, 0x2e, 0x0a),
        text: c(0xff, 0xe8, 0xcc),
        sub: c(0xc9, 0x9a, 0x6b),
        accent: c(0xff, 0xa7, 0x26),
        accent_dark: c(0xf4, 0x51, 0x1e),
        hi: c(0xff, 0xd1, 0x80),
        on_accent: c(0x12, 0x07, 0x00),
    },
    Palette {
        name: "03 펌프킨패치 라이트",
        tag: "PATCH · LIGHT",
        light: true,
        fog: false,
        bg: c(0xff, 0xf6, 0xe8),
        panel: c(0xff, 0xff, 0xff),
        panel2: c(0xff, 0xf0, 0xda),
        line: c(0xf3, 0xcf, 0xa0),
        text: c(0x3b, 0x23, 0x14),
        sub: c(0x8a, 0x61, 0x40),
        accent: c(0xe8, 0x59, 0x0c),
        accent_dark: c(0xff, 0x92, 0x2b),
        hi: c(0xc2, 0x4a, 0x00),
        on_accent: Color32::WHITE,
    },
    Palette {
        name: "04 프리미엄 느와르 오렌지",
        tag: "NOIR · ORANGE · PUMPKIN",
        light: false,
        fog: false,
        bg: c(0x0b, 0x09, 0x07),
        panel: c(0x1b, 0x14, 0x0d),
        panel2: c(0x12, 0x0d, 0x09),
        line: c(0x5c, 0x3a, 0x1c),
        text: c(0xf4, 0xea, 0xd8),
        sub: c(0xa8, 0x97, 0x7c),
        accent: c(0xff, 0x9a, 0x3c),
        accent_dark: c(0xc2, 0x4a, 0x00),
        hi: c(0xff, 0xc2, 0x7a),
        on_accent: c(0x1a, 0x0f, 0x02),
    },
    Palette {
        name: "05 프리미엄 벨벳 오렌지",
        tag: "VELVET · ORANGE · PUMPKIN",
        light: false,
        fog: false,
        bg: c(0x12, 0x07, 0x09),
        panel: c(0x34, 0x14, 0x18),
        panel2: c(0x26, 0x0e, 0x11),
        line: c(0x75, 0x3a, 0x24),
        text: c(0xfb, 0xea, 0xdf),
        sub: c(0xb9, 0x94, 0x86),
        accent: c(0xff, 0x8a, 0x3d),
        accent_dark: c(0xb8, 0x40, 0x0a),
        hi: c(0xff, 0xb3, 0x6b),
        on_accent: c(0x22, 0x0a, 0x00),
    },
    Palette {
        name: "06 안개 묘지 오렌지",
        tag: "MIST · GRAVEYARD · EMBER",
        light: false,
        fog: true,
        bg: c(0x2b, 0x17, 0x09),
        panel: c(0x42, 0x22, 0x10),
        panel2: c(0x37, 0x1b, 0x0c),
        line: c(0xaa, 0x6d, 0x3e),
        text: c(0xff, 0xfa, 0xf3),
        sub: c(0xf3, 0xd5, 0xbb),
        accent: c(0xff, 0x7a, 0x1a),
        accent_dark: c(0xc2, 0x4a, 0x00),
        hi: c(0xff, 0xc2, 0x7a),
        on_accent: c(0x1a, 0x09, 0x00),
    },
    Palette {
        name: "07 안개 유령저택 오렌지",
        tag: "MIST · MANOR · EMBER",
        light: false,
        fog: true,
        bg: c(0x34, 0x12, 0x0a),
        panel: c(0x4e, 0x20, 0x10),
        panel2: c(0x3f, 0x19, 0x0c),
        line: c(0xb5, 0x64, 0x35),
        text: c(0xff, 0xfa, 0xf3),
        sub: c(0xf5, 0xcd, 0xb0),
        accent: c(0xff, 0x7a, 0x1a),
        accent_dark: c(0xc2, 0x4a, 0x00),
        hi: c(0xff, 0xc2, 0x7a),
        on_accent: c(0x1a, 0x09, 0x00),
    },
];

/// 앱 아이콘 원본(pumpkin_premium.png를 256px로 줄인 것). 창 아이콘과 헤더 로고에 쓴다.
pub const ICON_PNG: &[u8] = include_bytes!("../assets/icon-256.png");

pub const OK: Color32 = c(0x8f, 0xd1, 0x9e);
pub const ERR: Color32 = c(0xef, 0x7b, 0x72);

impl Palette {
    pub fn warn(&self) -> Color32 {
        self.hi
    }
}

pub fn apply(ctx: &egui::Context, p: &Palette) {
    // 테마를 고정해 OS 다크/라이트 전환 시 기본 비주얼로 돌아가지 않게 한다.
    ctx.set_theme(if p.light { egui::Theme::Light } else { egui::Theme::Dark });
    let mut v = if p.light {
        egui::Visuals::light()
    } else {
        egui::Visuals::dark()
    };
    v.panel_fill = p.bg;
    v.window_fill = p.panel;
    v.window_stroke = Stroke::new(1.0, p.line);
    v.extreme_bg_color = p.panel2;
    v.faint_bg_color = p.panel2;
    v.hyperlink_color = p.accent;
    v.selection.bg_fill = p.accent_dark;
    v.selection.stroke = Stroke::new(1.0, p.hi);
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(10);

    let r = CornerRadius::same(8);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.hovered.fg_stroke = Stroke::new(1.5, if p.light { Color32::WHITE } else { p.hi });
    v.widgets.active.fg_stroke = Stroke::new(2.0, p.on_accent);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.line);
    v.widgets.noninteractive.corner_radius = r;
    v.widgets.inactive.weak_bg_fill = p.panel2;
    v.widgets.inactive.bg_fill = p.panel2;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.line);
    v.widgets.inactive.corner_radius = r;
    v.widgets.hovered.weak_bg_fill = p.accent_dark;
    v.widgets.hovered.bg_stroke = Stroke::new(1.5, p.accent);
    v.widgets.hovered.corner_radius = r;
    v.widgets.active.weak_bg_fill = p.accent;
    v.widgets.active.bg_stroke = Stroke::new(1.5, p.hi);
    v.widgets.active.corner_radius = r;
    v.widgets.open.corner_radius = r;
    ctx.set_visuals(v);

    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 7.0);
        s.spacing.button_padding = egui::vec2(10.0, 5.0);
    });
}

/// 샘플의 `section` 카드: 둥근 모서리, 얇은 테두리, 그림자.
pub fn card(p: &Palette) -> Frame {
    Frame::new()
        .fill(p.panel)
        .stroke(Stroke::new(1.0, p.line))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::same(14))
        .shadow(Shadow {
            offset: [0, 8],
            blur: 24,
            spread: 0,
            color: Color32::from_black_alpha(if p.light { 30 } else { 110 }),
        })
}

/// 패널 바깥 여백(투명: 아래의 paint_backdrop 그라데이션이 보이게 한다).
pub fn backdrop(_p: &Palette) -> Frame {
    Frame::new().inner_margin(Margin::same(8))
}

/// `I · 데이터 소스` 같은 섹션 제목과 밑줄.
pub fn section_title(ui: &mut Ui, p: &Palette, roman: &str, title: &str) {
    ui.add_space(2.0);
    ui.label(
        RichText::new(format!("{roman} · {title}"))
            .color(p.accent)
            .size(15.0)
            .strong(),
    );
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 2.0), egui::Sense::hover());
    paint_h_gradient(ui.painter(), rect, p.accent, Color32::TRANSPARENT);
    ui.add_space(4.0);
}

/// 주요 버튼: 채워진 주황 + 바깥 글로우.
pub fn primary_button(ui: &mut Ui, p: &Palette, text: &str, enabled: bool) -> egui::Response {
    let btn = egui::Button::new(RichText::new(text).color(p.on_accent).strong())
        .fill(p.accent)
        .stroke(Stroke::new(1.0, p.hi))
        .corner_radius(CornerRadius::same(10));
    let resp = ui.add_enabled(enabled, btn);
    if enabled {
        for (grow, alpha) in [(2.0, 90u8), (4.5, 50), (7.0, 22)] {
            ui.painter().rect_stroke(
                resp.rect.expand(grow),
                CornerRadius::same(12 + grow as u8),
                Stroke::new(1.5, p.accent.gamma_multiply(alpha as f32 / 255.0)),
                egui::StrokeKind::Outside,
            );
        }
    }
    resp
}

pub fn paint_h_gradient(painter: &Painter, rect: Rect, left: Color32, right: Color32) {
    let mut m = Mesh::default();
    let (a, b, c2, d) = (
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
    );
    m.colored_vertex(a, left);
    m.colored_vertex(b, right);
    m.colored_vertex(c2, right);
    m.colored_vertex(d, left);
    m.add_triangle(0, 1, 2);
    m.add_triangle(0, 2, 3);
    painter.add(m);
}

fn paint_v_gradient(painter: &Painter, rect: Rect, top: Color32, bottom: Color32) {
    let mut m = Mesh::default();
    m.colored_vertex(rect.left_top(), top);
    m.colored_vertex(rect.right_top(), top);
    m.colored_vertex(rect.right_bottom(), bottom);
    m.colored_vertex(rect.left_bottom(), bottom);
    m.add_triangle(0, 1, 2);
    m.add_triangle(0, 2, 3);
    painter.add(m);
}

/// 샘플 04~07의 호박 로고를 도형으로 그린다.
pub fn pumpkin_logo(painter: &Painter, center: Pos2, size: f32, p: &Palette) {
    let s = size / 100.0;
    let at = |x: f32, y: f32| center + egui::vec2((x - 50.0) * s, (y - 50.0) * s);
    let ellipse = |cx: f32, cy: f32, rx: f32, ry: f32, col: Color32| {
        let pts: Vec<Pos2> = (0..32)
            .map(|i| {
                let a = i as f32 / 32.0 * std::f32::consts::TAU;
                at(cx + rx * a.cos(), cy + ry * a.sin())
            })
            .collect();
        painter.add(egui::Shape::convex_polygon(pts, col, Stroke::NONE));
    };
    painter.rect_filled(
        Rect::from_min_max(at(46.0, 6.0), at(55.0, 24.0)),
        3.0 * s,
        if p.fog { c(0x4d, 0x5a, 0x3a) } else { p.hi },
    );
    ellipse(30.0, 58.0, 24.0, 34.0, p.accent_dark);
    ellipse(70.0, 58.0, 24.0, 34.0, p.accent_dark);
    ellipse(50.0, 58.0, 22.0, 36.0, p.accent);
    let face = if p.fog { c(0xff, 0xf0, 0xc2) } else { p.bg };
    painter.add(egui::Shape::convex_polygon(
        vec![at(34.0, 48.0), at(44.0, 48.0), at(39.0, 38.0)],
        face,
        Stroke::NONE,
    ));
    painter.add(egui::Shape::convex_polygon(
        vec![at(56.0, 48.0), at(66.0, 48.0), at(61.0, 38.0)],
        face,
        Stroke::NONE,
    ));
    painter.add(egui::Shape::convex_polygon(
        vec![
            at(33.0, 66.0),
            at(40.0, 74.0),
            at(46.0, 68.0),
            at(52.0, 74.0),
            at(58.0, 68.0),
            at(65.0, 74.0),
            at(67.0, 66.0),
            at(50.0, 78.0),
        ],
        face,
        Stroke::NONE,
    ));
}

/// 창 전체 배경: 위에서 아래로 번지는 그라데이션과 오른쪽 위 주황 글로우.
pub fn paint_backdrop(painter: &Painter, rect: Rect, p: &Palette) {
    let lift = if p.light {
        p.bg
    } else {
        p.bg.lerp_to_gamma(p.accent_dark, 0.12)
    };
    paint_v_gradient(painter, rect, p.bg, lift);
    if !p.light {
        painter.circle_filled(
            Pos2::new(rect.right() - 120.0, rect.top() + 20.0),
            380.0,
            p.accent.gamma_multiply(0.035),
        );
    }
}

/// 안개 테마(06/07): 아래에서 올라오는 주황빛 안개와 천천히 흐르는 연기 덩어리.
pub fn paint_fog(ctx: &egui::Context, p: &Palette) {
    if !p.fog {
        return;
    }
    let screen = ctx.content_rect();
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Middle, egui::Id::unique("fog")));
    let t = ctx.input(|i| i.time) as f32;
    let glow = Rect::from_min_max(Pos2::new(screen.left(), screen.bottom() - 220.0), screen.right_bottom());
    paint_v_gradient(&painter, glow, Color32::TRANSPARENT, p.accent.gamma_multiply(0.16));
    for i in 0..7 {
        let f = i as f32;
        let x = screen.left() + ((t * (6.0 + f * 1.7) + f * 260.0) % (screen.width() + 400.0)) - 200.0;
        let y = screen.bottom() - 20.0 - (f * 17.0) % 80.0;
        painter.circle_filled(Pos2::new(x, y), 100.0 + f * 12.0, p.accent.gamma_multiply(0.03));
    }
    if ctx.input(|i| i.focused) {
        ctx.request_repaint_after(std::time::Duration::from_millis(50));
    }
}
