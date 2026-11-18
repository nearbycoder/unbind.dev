use std::sync::OnceLock;

use rune::{
    Color, Cursor, Cx, El, FileDialogOptions, ScrollSpec, Shape, TextEdit, TextStyle, Tx, col, el,
    row, text, text_input,
};

use super::state::{Page, Session, SessionInvite, SessionPeer, State};

const BODY: TextStyle = TextStyle::new(14.0, 400.0).lh(21.0);
const LABEL: TextStyle = TextStyle::new(13.0, 600.0);
const SMALL: TextStyle = TextStyle::new(12.0, 450.0).lh(18.0);
const CODE: TextStyle = TextStyle::new(13.0, 450.0).mono();
const TITLE: TextStyle = TextStyle::new(34.0, 700.0).tracking(-1.5);
const SECTION: TextStyle = TextStyle::new(17.0, 650.0).tracking(-0.35);

struct Palette {
    background: Color,
    sidebar: Color,
    panel: Color,
    field: Color,
    border: Color,
    ink: Color,
    muted: Color,
    accent: Color,
    accent_surface: Color,
    green: Color,
    danger: Color,
}

fn palette() -> &'static Palette {
    static PALETTE: OnceLock<Palette> = OnceLock::new();
    PALETTE.get_or_init(|| Palette {
        background: color("#161412"),
        sidebar: color("#1B1816"),
        panel: color("#211D1A"),
        field: color("#151311"),
        border: color("#3A322D"),
        ink: color("#FFF4E8"),
        muted: color("#BBAE9F"),
        accent: color("#FFA488"),
        accent_surface: color("#3B2922"),
        green: color("#A4D4AF"),
        danger: color("#FF9D94"),
    })
}

fn color(hex: &str) -> Color {
    rune::color::hex(hex).expect("view color is valid hexadecimal")
}

pub fn view(s: &State, cx: &mut Cx<State>) -> El {
    let p = palette();
    cx.foreground(p.ink);
    cx.focus_ring(2.0, 2.0, p.accent);

    let (title, subtitle, content) = match s.page {
        Page::Share => (
            "Share a service",
            "Create a private invite for something running on this computer.",
            share_view(s),
        ),
        Page::Connect => (
            "Connect to a service",
            "Use a host’s invite to access their service on a local port.",
            connect_view(s),
        ),
        Page::Files => (
            "Transfer files",
            "Send a file directly to another computer or receive one from an invite.",
            files_view(s),
        ),
        Page::Sessions => (
            "Sessions",
            "See your shares, connections, and pending approvals.",
            sessions_view(s),
        ),
        Page::Settings => (
            "Settings",
            "Configure how Unbind finds and connects to peers.",
            settings_view(s),
        ),
    };

    let mut main = col().w_full().min_w(0.0).padding(30.0).gap(20.0).child(
        col()
            .gap(7.0)
            .child(heading(title, 1))
            .child(text(subtitle).font(BODY).color(p.muted).wrap()),
    );
    if let Some(notice) = &s.notice {
        let tone = if notice.error { p.danger } else { p.green };
        main = main.child(
            row()
                .key("notice")
                .role(if notice.error { "alert" } else { "status" })
                .center_y()
                .gap(10.0)
                .px(13.0)
                .py(9.0)
                .radius(8.0)
                .border(1.0, tone.alpha(0.26))
                .fill(tone.alpha(0.08))
                .child(el().size(6.0, 6.0).radius(3.0).fill(tone).a11y_hidden())
                .child(
                    text(&notice.text)
                        .font(SMALL)
                        .color(tone)
                        .wrap_anywhere()
                        .min_w(0.0),
                )
                .child(button(
                    "dismiss-notice",
                    "Dismiss",
                    ButtonStyle::Quiet,
                    false,
                    |s, _| {
                        s.notice = None;
                    },
                )),
        );
    } else if s.busy {
        main = main.child(description("Working…").key("working").role("status"));
    }
    if let Some(invite) = &s.invite {
        main = main.child(invite_view(invite));
    }
    main = main.child(content);

    row()
        .key("unbind")
        .w_full()
        .h_full()
        .fill(p.background)
        .child(sidebar(s))
        .child(
            col()
                .key(match s.page {
                    Page::Share => "share-scroll",
                    Page::Connect => "connect-scroll",
                    Page::Files => "files-scroll",
                    Page::Sessions => "sessions-scroll",
                    Page::Settings => "settings-scroll",
                })
                .role("main")
                .h_full()
                .grow(1.0)
                .min_w(0.0)
                .min_h(0.0)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .scroll_margin(12.0)
                .child(main),
        )
}

fn sidebar(s: &State) -> El {
    let p = palette();
    let active_sessions = s.sessions.len();
    let summary = match active_sessions {
        0 => "No active sessions".to_owned(),
        1 => "1 active session".to_owned(),
        count => format!("{count} active sessions"),
    };
    col()
        .key("sidebar")
        .role("navigation")
        .label("Unbind views")
        .w(184.0)
        .h_full()
        .gap(23.0)
        .px(14.0)
        .py(24.0)
        .fill(p.sidebar)
        .border_sides([0.0, 1.0, 0.0, 0.0], p.border)
        .child(
            row()
                .center_y()
                .gap(10.0)
                .px(5.0)
                .child(logo(s.logo.as_ref()))
                .child(
                    text("unbind")
                        .font(TextStyle::new(26.0, 750.0).tracking(-1.6))
                        .color(p.ink),
                ),
        )
        .child(col().gap(5.0).children([
            navigation(s, Page::Share, "Share", "nav-share"),
            navigation(s, Page::Connect, "Connect", "nav-connect"),
            navigation(s, Page::Files, "Files", "nav-files"),
            navigation(s, Page::Sessions, "Sessions", "nav-sessions"),
            navigation(s, Page::Settings, "Settings", "nav-settings"),
        ]))
        .child(el().grow(1.0))
        .child(
            col()
                .gap(8.0)
                .px(5.0)
                .child(el().h(1.0).fill(p.border).a11y_hidden())
                .child(
                    row()
                        .center_y()
                        .gap(7.0)
                        .child(
                            el().size(6.0, 6.0)
                                .radius(3.0)
                                .fill(if active_sessions == 0 {
                                    p.muted
                                } else {
                                    p.green
                                })
                                .a11y_hidden(),
                        )
                        .child(text(summary).font(SMALL).color(p.muted)),
                )
                .child(
                    text(concat!("Version ", env!("CARGO_PKG_VERSION")))
                        .font(SMALL)
                        .color(p.muted),
                ),
        )
}

fn navigation(s: &State, page: Page, label: &'static str, key: &'static str) -> El {
    let p = palette();
    let active = s.page == page;
    let ink = if active { p.accent } else { p.muted };
    row()
        .key(key)
        .role("button")
        .label(label)
        .checked(active)
        .focusable(true)
        .h(39.0)
        .radius(8.0)
        .px(10.0)
        .gap(10.0)
        .center_y()
        .fill(if active {
            p.accent_surface
        } else {
            Color::TRANSPARENT
        })
        .hover(p.ink.alpha(0.07))
        .press(p.ink.alpha(0.12))
        .cursor(Cursor::Pointer)
        .child(nav_icon(page, ink))
        .child(
            text(label)
                .font(LABEL)
                .color(if active { p.ink } else { p.muted })
                .a11y_hidden(),
        )
        .on_press(move |s: &mut State| {
            s.page = page;
            if page == Page::Sessions {
                s.refresh_sessions();
            }
        })
}

fn logo(image: Option<&rune::Image>) -> El {
    if let Some(image) = image.filter(|image| image.is_ready() && image.error().is_none()) {
        return el().size(35.0, 35.0).pixels(image.pixels()).a11y_hidden();
    }
    let ink = color("#241918");
    let outline = rune::Path::new()
        .move_to([25.0, 18.0])
        .line_to([21.0, 18.0])
        .quad_to([15.0, 18.0], [15.0, 24.0])
        .line_to([15.0, 40.0])
        .quad_to([15.0, 46.0], [21.0, 46.0])
        .line_to([25.0, 46.0])
        .move_to([39.0, 18.0])
        .line_to([43.0, 18.0])
        .quad_to([49.0, 18.0], [49.0, 24.0])
        .line_to([49.0, 40.0])
        .quad_to([49.0, 46.0], [43.0, 46.0])
        .line_to([39.0, 46.0])
        .move_to([25.0, 39.0])
        .line_to([39.0, 25.0]);
    el().size(35.0, 35.0)
        .radius(9.0)
        .gradient(color("#FFC0A4"), color("#FF8C70"))
        .shape(
            Shape::new()
                .stroke_path(&outline, 2.6, rune::LineCap::Round, ink)
                .view_box(64.0, 64.0),
        )
        .a11y_hidden()
}

fn nav_icon(page: Page, ink: Color) -> El {
    let outline = match page {
        Page::Share => Shape::new()
            .stroke([[9.0, 12.0], [16.0, 5.0]], 1.5, ink)
            .stroke([[11.0, 5.0], [16.0, 5.0], [16.0, 10.0]], 1.5, ink)
            .stroke(
                [
                    [12.0, 5.0],
                    [4.0, 5.0],
                    [4.0, 16.0],
                    [16.0, 16.0],
                    [16.0, 12.0],
                ],
                1.5,
                ink,
            ),
        Page::Connect => Shape::new()
            .stroke([[17.0, 5.0], [10.0, 12.0]], 1.5, ink)
            .stroke([[10.0, 7.0], [10.0, 12.0], [15.0, 12.0]], 1.5, ink)
            .stroke(
                [
                    [12.0, 5.0],
                    [4.0, 5.0],
                    [4.0, 16.0],
                    [16.0, 16.0],
                    [16.0, 12.0],
                ],
                1.5,
                ink,
            ),
        Page::Files => Shape::new()
            .stroke(
                [
                    [5.0, 3.0],
                    [12.0, 3.0],
                    [16.0, 7.0],
                    [16.0, 17.0],
                    [5.0, 17.0],
                    [5.0, 3.0],
                ],
                1.5,
                ink,
            )
            .stroke([[11.5, 3.5], [11.5, 7.5], [15.5, 7.5]], 1.5, ink)
            .stroke([[8.0, 12.5], [13.0, 12.5]], 1.5, ink),
        Page::Sessions => Shape::new().stroke(
            [
                [2.5, 10.0],
                [6.0, 10.0],
                [8.0, 5.0],
                [11.0, 15.0],
                [13.0, 10.0],
                [17.5, 10.0],
            ],
            1.5,
            ink,
        ),
        Page::Settings => Shape::new()
            .stroke([[4.0, 5.0], [16.0, 5.0]], 1.5, ink)
            .stroke([[4.0, 10.0], [16.0, 10.0]], 1.5, ink)
            .stroke([[4.0, 15.0], [16.0, 15.0]], 1.5, ink)
            .stroke([[8.0, 3.5], [8.0, 6.5]], 2.3, ink)
            .stroke([[13.0, 8.5], [13.0, 11.5]], 2.3, ink)
            .stroke([[7.0, 13.5], [7.0, 16.5]], 2.3, ink),
    };
    el().size(20.0, 20.0).shape(outline).a11y_hidden()
}

fn heading(title: &str, level: u32) -> El {
    text(title)
        .role("heading")
        .level(level)
        .font(if level == 1 { TITLE } else { SECTION })
        .color(palette().ink)
        .wrap()
}

fn description(value: impl Into<String>) -> El {
    text(value).font(SMALL).color(palette().muted).wrap()
}

fn card() -> El {
    let p = palette();
    col()
        .min_w(0.0)
        .padding(18.0)
        .gap(13.0)
        .radius(11.0)
        .fill(p.panel)
        .border(1.0, p.border)
}

fn line() -> El {
    el().h(1.0).fill(palette().border).a11y_hidden()
}

#[derive(Clone, Copy)]
enum ButtonStyle {
    Primary,
    Secondary,
    Quiet,
    Danger,
}

fn button(
    key: impl Into<String>,
    label: impl Into<String>,
    style: ButtonStyle,
    disabled: bool,
    pressed: impl Fn(&mut State, &mut Tx) + 'static,
) -> El {
    let p = palette();
    let (background, foreground, stroke) = match style {
        ButtonStyle::Primary => (p.accent, p.field, p.accent),
        ButtonStyle::Secondary => (p.panel, p.ink, p.border),
        ButtonStyle::Quiet => (Color::TRANSPARENT, p.muted, Color::TRANSPARENT),
        ButtonStyle::Danger => (Color::TRANSPARENT, p.danger, p.danger.alpha(0.30)),
    };
    let label = label.into();
    let base = row()
        .key(key)
        .role("button")
        .label(&label)
        .min_h(36.0)
        .center()
        .px(13.0)
        .py(7.0)
        .radius(7.0)
        .fill(background)
        .border(1.0, stroke)
        .child(text(label).font(LABEL).color(foreground).a11y_hidden());
    if disabled {
        base.opacity(0.50).disabled(true)
    } else {
        base.focusable(true)
            .cursor(Cursor::Pointer)
            .hover(foreground.alpha(0.09))
            .press(foreground.alpha(0.16))
            .on_press_tx(pressed)
    }
}

#[derive(Clone, Copy)]
enum FieldStyle {
    Line,
    Secret,
    ConnectInvite,
    Routes,
}

fn field(
    key: &'static str,
    label: &'static str,
    hint: &'static str,
    value: &TextEdit,
    style: FieldStyle,
    edit: for<'a> fn(&'a mut State) -> &'a mut TextEdit,
) -> El {
    let p = palette();
    let mut input = text_input(value, CODE.lh(20.0))
        .key(key)
        .label(label)
        .w_full()
        .min_w(0.0)
        .placeholder(hint, p.muted)
        .color(p.ink)
        .caret(p.accent, p.accent.alpha(0.30))
        .padding(10.0)
        .radius(7.0)
        .fill(p.field)
        .border(1.0, p.border)
        .focusable(true)
        .on_edit(edit);
    input = match style {
        FieldStyle::Line => input.h(40.0).single_line(),
        FieldStyle::Secret => input.h(40.0).single_line().secure(),
        FieldStyle::ConnectInvite => input
            .h(40.0)
            .single_line()
            .secure()
            .on_input_change(|s: &mut State, _| s.connect.origin = None),
        FieldStyle::Routes => input.min_h(64.0).wrap_anywhere(),
    };
    col()
        .gap(5.0)
        .min_w(0.0)
        .child(text(label).font(LABEL).color(p.ink).a11y_hidden())
        .child(input)
}

fn toggle(key: &'static str, label: &'static str, on: bool, flip: fn(&mut State)) -> El {
    let p = palette();
    let track = row()
        .size(30.0, 17.0)
        .px(2.0)
        .center_y()
        .radius(9.0)
        .fill(if on { p.accent } else { p.border })
        .child(el().w(if on { 13.0 } else { 0.0 }))
        .child(
            el().size(13.0, 13.0)
                .radius(7.0)
                .fill(if on { p.field } else { p.ink }),
        )
        .a11y_hidden();
    row()
        .key(key)
        .role("switch")
        .label(label)
        .checked(on)
        .focusable(true)
        .cursor(Cursor::Pointer)
        .min_h(34.0)
        .center_y()
        .justify_between()
        .gap(12.0)
        .px(7.0)
        .py(3.0)
        .radius(6.0)
        .hover(p.ink.alpha(0.05))
        .child(
            text(label)
                .font(LABEL)
                .color(p.ink)
                .wrap()
                .min_w(0.0)
                .a11y_hidden(),
        )
        .child(track)
        .on_press(flip)
}

fn advanced_button(
    key: &'static str,
    label: &'static str,
    expanded: bool,
    pressed: fn(&mut State),
) -> El {
    button(
        key,
        format!("{label} {}", if expanded { "−" } else { "+" }),
        ButtonStyle::Secondary,
        false,
        move |s, _| pressed(s),
    )
    .label(label)
    .expanded(expanded)
}

fn share_view(s: &State) -> El {
    let mut services = card()
        .child(heading("Local service", 2))
        .child(field(
            "share-target",
            "Port or address",
            "3000 or localhost:3000",
            &s.share.target,
            FieldStyle::Line,
            |s| &mut s.share.target,
        ))
        .child(description(
            "Use a port number for a service on this computer.",
        ));
    if s.share.advanced {
        services = services
            .child(line())
            .child(field(
                "share-tcp",
                "Named TCP services",
                "web=3000",
                &s.share.tcp,
                FieldStyle::Routes,
                |s| &mut s.share.tcp,
            ))
            .child(field(
                "share-udp",
                "Named UDP services",
                "game=27015",
                &s.share.udp,
                FieldStyle::Routes,
                |s| &mut s.share.udp,
            ))
            .child(description("Enter one NAME=ADDRESS route per line."))
            .child(toggle(
                "share-lan",
                "Local network only",
                s.share.lan,
                |s| {
                    s.share.lan = !s.share.lan;
                },
            ));
    }
    let form = col()
        .min_w(364.0)
        .grow(1.0)
        .gap(14.0)
        .child(services)
        .child(
            card().child(heading("Invite access", 2)).child(
                row()
                    .gap(17.0)
                    .child(
                        col()
                            .min_w(0.0)
                            .grow(1.0)
                            .gap(3.0)
                            .child(toggle(
                                "share-single",
                                "One guest only",
                                s.share.single_use,
                                |s| {
                                    s.share.single_use = !s.share.single_use;
                                },
                            ))
                            .child(toggle(
                                "share-approve",
                                "Approve each guest",
                                s.share.approve,
                                |s| {
                                    s.share.approve = !s.share.approve;
                                },
                            )),
                    )
                    .child(
                        field(
                            "share-expires",
                            "Expires after (minutes)",
                            "No time limit",
                            &s.share.expires,
                            FieldStyle::Line,
                            |s| &mut s.share.expires,
                        )
                        .w(156.0),
                    ),
            ),
        )
        .child(
            row()
                .gap(9.0)
                .flex_wrap()
                .child(
                    button(
                        "share-start",
                        "Create invite",
                        ButtonStyle::Primary,
                        s.busy,
                        |s, _| {
                            s.submit_share();
                        },
                    )
                    .default_button(),
                )
                .child(advanced_button(
                    "share-advanced",
                    "Advanced sharing",
                    s.share.advanced,
                    |s| {
                        s.share.advanced = !s.share.advanced;
                    },
                )),
        );
    col()
        .gap(16.0)
        .child(
            row()
                .gap(14.0)
                .flex_wrap()
                .items_start()
                .child(form)
                .child(connection_path(s)),
        )
        .child(profiles_view(s))
}

fn connection_path(s: &State) -> El {
    let p = palette();
    let ending = if s.share.lan {
        "On your local network"
    } else {
        "Encrypted between peers"
    };
    let panel = |at: Page, title: &str, detail: &str| {
        row()
            .gap(8.0)
            .center_y()
            .px(9.0)
            .py(8.0)
            .radius(7.0)
            .fill(p.field)
            .border(1.0, p.border)
            .child(nav_icon(at, p.accent))
            .child(
                col()
                    .gap(2.0)
                    .child(text(title).font(LABEL).color(p.ink))
                    .child(description(detail)),
            )
    };
    card()
        .w(223.0)
        .gradient(color("#2D221D"), p.panel)
        .child(heading("Connection path", 2))
        .child(panel(Page::Share, "This computer", "Runs the service"))
        .child(
            row()
                .center_y()
                .gap(8.0)
                .pl(10.0)
                .child(
                    el().size(2.0, 24.0)
                        .radius(1.0)
                        .fill(p.accent)
                        .a11y_hidden(),
                )
                .child(text("Direct tunnel").font(SMALL).color(p.accent)),
        )
        .child(panel(Page::Connect, "Guest computer", "Uses a local port"))
        .child(description(ending))
}

fn profiles_view(s: &State) -> El {
    let mut panel = card()
        .child(heading("Saved profiles", 2))
        .child(description(
            "Save this setup so you can use the same ports and access settings later.",
        ))
        .child(
            row()
                .gap(9.0)
                .items_end()
                .child(
                    field(
                        "profile-name",
                        "Profile name",
                        "development-server",
                        &s.share.profile,
                        FieldStyle::Line,
                        |s| &mut s.share.profile,
                    )
                    .grow(1.0),
                )
                .child(button(
                    "profile-save",
                    "Save profile",
                    ButtonStyle::Secondary,
                    s.busy,
                    |s, _| {
                        s.save_profile();
                    },
                )),
        );
    if !s.profiles.is_empty() {
        panel = panel
            .child(line())
            .child(col().gap(5.0).children(s.profiles.iter().map(|profile| {
                let name = profile.clone();
                let key = format!("profile-load-{profile}");
                row()
                    .gap(12.0)
                    .center_y()
                    .justify_between()
                    .child(text(profile).font(BODY).wrap_anywhere().min_w(0.0))
                    .child(
                        button(key, "Load", ButtonStyle::Quiet, s.busy, move |s, _| {
                            s.load_profile(&name);
                        })
                        .label(format!("Load profile {profile}")),
                    )
            })));
    }
    panel
}

fn connect_view(s: &State) -> El {
    let mut options = card()
        .child(heading("Local access", 2))
        .child(field(
            "connect-listen",
            "Listen on",
            "8080 or 127.0.0.1:8080",
            &s.connect.listen,
            FieldStyle::Line,
            |s| &mut s.connect.listen,
        ))
        .child(description(
            "Open this local address in your browser or point an SSH client at it.",
        ))
        .child(toggle(
            "connect-identify",
            "Identify this device to the host",
            s.connect.identify,
            |s| {
                s.connect.identify = !s.connect.identify;
            },
        ))
        .child(toggle(
            "connect-retry",
            "Reconnect after an interruption",
            s.connect.retry,
            |s| {
                s.connect.retry = !s.connect.retry;
            },
        ));
    if s.connect.advanced {
        options = options
            .child(line())
            .child(field(
                "connect-tcp",
                "Named TCP ports",
                "web=8080",
                &s.connect.tcp,
                FieldStyle::Routes,
                |s| &mut s.connect.tcp,
            ))
            .child(field(
                "connect-udp",
                "Named UDP ports",
                "game=27015",
                &s.connect.udp,
                FieldStyle::Routes,
                |s| &mut s.connect.udp,
            ))
            .child(description(
                "Match the host’s named routes, one NAME=LOCAL_PORT per line.",
            ));
    }

    let mut invite = card().child(
        row()
            .gap(9.0)
            .items_end()
            .child(
                field(
                    "connect-link",
                    "Invite link",
                    "Paste an invite URL",
                    &s.connect.link,
                    FieldStyle::ConnectInvite,
                    |s| &mut s.connect.link,
                )
                .grow(1.0),
            )
            .child(paste_button("connect-paste", |s, link| {
                s.connect.link.replace_text(link);
                s.connect.origin = None;
            })),
    );
    if let Some(origin) = &s.connect.origin {
        invite = invite.child(description(format!("Invite server • {origin}")));
    }
    col().gap(14.0).child(invite).child(options).child(
        row()
            .gap(9.0)
            .flex_wrap()
            .child(
                button(
                    "connect-start",
                    "Connect",
                    ButtonStyle::Primary,
                    s.busy,
                    |s, _| {
                        s.submit_connect();
                    },
                )
                .default_button(),
            )
            .child(advanced_button(
                "connect-advanced",
                "Named routes",
                s.connect.advanced,
                |s| {
                    s.connect.advanced = !s.connect.advanced;
                },
            )),
    )
}

fn paste_button(key: &'static str, paste: fn(&mut State, &str)) -> El {
    button(key, "Paste", ButtonStyle::Secondary, false, move |_, tx| {
        tx.read_clipboard(move |s: &mut State, content, _| {
            let link = content.text().trim();
            if link.is_empty() {
                s.set_error("The clipboard doesn’t contain an invite link.");
            } else {
                paste(s, link);
            }
        });
    })
    .label("Paste invite from clipboard")
}

fn invite_view(invite: &super::state::InviteDisplay) -> El {
    let p = palette();
    let show_qr = invite.show_qr;
    let mut panel = card()
        .key("current-invite")
        .border(1.0, p.accent.alpha(0.45))
        .gradient(p.accent_surface, p.panel)
        .child(heading("Invite ready", 2))
        .child(description(format!(
            "Send the link to a guest you trust. Invite server • {}",
            invite.origin
        )))
        .child(
            row()
                .gap(8.0)
                .flex_wrap()
                .child(button(
                    "invite-copy",
                    "Copy invite",
                    ButtonStyle::Primary,
                    false,
                    |s, tx| {
                        s.copy_invite(tx);
                    },
                ))
                .child(button(
                    "invite-qr",
                    if show_qr {
                        "Hide QR code"
                    } else {
                        "Show QR code"
                    },
                    ButtonStyle::Secondary,
                    false,
                    |s, _| {
                        if let Some(invite) = s.invite.as_mut() {
                            invite.show_qr = !invite.show_qr;
                        }
                    },
                ))
                .child(button(
                    "invite-dismiss",
                    "Dismiss",
                    ButtonStyle::Quiet,
                    false,
                    |s, _| {
                        s.invite = None;
                    },
                )),
        );
    if show_qr {
        panel = match &invite.qr {
            Some(qr) => panel.child(
                el().size(174.0, 174.0)
                    .radius(8.0)
                    .fill(Color::WHITE)
                    .padding(8.0)
                    .role("img")
                    .label("Invite QR code")
                    .pixels(qr.pixels()),
            ),
            None => panel.child(description(
                "This invite has no QR image. Copy the link to share it.",
            )),
        };
    }
    panel
}

fn files_view(s: &State) -> El {
    let send = card()
        .child(heading("Send a file", 2))
        .child(description(
            "Create an invite that one recipient can use to download your file.",
        ))
        .child(
            row()
                .gap(9.0)
                .items_end()
                .child(
                    field(
                        "file-send-path",
                        "File to send",
                        "Choose a file or enter its path",
                        &s.files.send_path,
                        FieldStyle::Line,
                        |s| &mut s.files.send_path,
                    )
                    .grow(1.0),
                )
                .child(button(
                    "file-browse-open",
                    "Choose file",
                    ButtonStyle::Secondary,
                    false,
                    |_, tx| {
                        tx.open_file(
                            FileDialogOptions {
                                title: "Choose a file to send".into(),
                                ..Default::default()
                            },
                            |s: &mut State, result, _| match result {
                                Ok(Some(path)) => s.choose_send_path(path),
                                Ok(None) => {}
                                Err(error) => s.set_error(error.to_string()),
                            },
                        );
                    },
                )),
        )
        .child(row().child(button(
            "file-send",
            "Create file invite",
            ButtonStyle::Primary,
            s.busy,
            |s, _| {
                s.send_file();
            },
        )));
    let receive = card()
        .child(heading("Receive a file", 2))
        .child(description(
            "Unbind checks the file’s SHA-256 before saving it.",
        ))
        .child(
            row()
                .gap(9.0)
                .items_end()
                .child(
                    field(
                        "file-link",
                        "File invite link",
                        "Paste a file invite",
                        &s.files.link,
                        FieldStyle::Secret,
                        |s| &mut s.files.link,
                    )
                    .grow(1.0),
                )
                .child(paste_button("file-paste", |s, link| {
                    s.files.link.replace_text(link)
                })),
        )
        .child(
            row()
                .gap(9.0)
                .items_end()
                .child(
                    field(
                        "file-receive-path",
                        "Save as",
                        "Choose a new destination",
                        &s.files.receive_path,
                        FieldStyle::Line,
                        |s| &mut s.files.receive_path,
                    )
                    .grow(1.0),
                )
                .child(button(
                    "file-browse-save",
                    "Choose location",
                    ButtonStyle::Secondary,
                    false,
                    |_, tx| {
                        tx.save_file(
                            FileDialogOptions {
                                title: "Save the incoming file".into(),
                                ..Default::default()
                            },
                            |s: &mut State, result, _| match result {
                                Ok(Some(path)) => s.choose_receive_path(path),
                                Ok(None) => {}
                                Err(error) => s.set_error(error.to_string()),
                            },
                        );
                    },
                )),
        )
        .child(toggle(
            "file-identify",
            "Identify this device to the sender",
            s.connect.identify,
            |s| s.connect.identify = !s.connect.identify,
        ))
        .child(row().child(button(
            "file-receive",
            "Receive file",
            ButtonStyle::Primary,
            s.busy,
            |s, _| {
                s.receive_file();
            },
        )));
    col().gap(14.0).child(send).child(receive)
}

fn sessions_view(s: &State) -> El {
    let p = palette();
    let mut content = col().gap(13.0).child(row().justify_end().child(button(
        "sessions-refresh",
        "Refresh",
        ButtonStyle::Secondary,
        s.busy,
        |s, _| {
            s.refresh_sessions();
        },
    )));
    if s.sessions.is_empty() {
        content = content.child(
            card()
                .padding(21.0)
                .child(nav_icon(Page::Sessions, p.accent))
                .child(heading("No active sessions", 2))
                .child(description("Active shares and connections appear here."))
                .child(
                    row()
                        .gap(9.0)
                        .flex_wrap()
                        .child(button(
                            "sessions-empty-share",
                            "Share a service",
                            ButtonStyle::Primary,
                            false,
                            |s, _| {
                                s.page = Page::Share;
                            },
                        ))
                        .child(button(
                            "sessions-empty-connect",
                            "Connect to a service",
                            ButtonStyle::Secondary,
                            false,
                            |s, _| {
                                s.page = Page::Connect;
                            },
                        )),
                ),
        );
    } else {
        content = content.children(
            s.sessions
                .iter()
                .map(|session| session_view(session, s.busy)),
        );
    }
    content
}

fn session_view(session: &Session, busy: bool) -> El {
    let p = palette();
    let sharing = session.kind == "share";
    let id = &session.id;
    let targets = if sharing {
        &session.targets
    } else {
        &session.listeners
    };
    let title = if targets.is_empty() {
        if sharing {
            "Share".to_owned()
        } else {
            "Connection".to_owned()
        }
    } else {
        targets.join(", ")
    };
    let label = if sharing {
        "Sharing"
    } else {
        session_state(&session.state)
    };
    let stop_id = id.clone();
    let mut actions = row().gap(7.0).flex_wrap();
    if sharing {
        let invite_id = id.clone();
        actions = actions.child(button(
            format!("session-{id}-invite"),
            "New invite",
            ButtonStyle::Secondary,
            busy,
            move |s, _| s.new_invite(&invite_id),
        ));
    }
    actions = actions.child(button(
        format!("session-{id}-stop"),
        "Stop",
        ButtonStyle::Danger,
        busy,
        move |s, _| s.stop(&stop_id),
    ));
    let mut panel = card().key(format!("session-{id}")).child(
        row()
            .gap(12.0)
            .justify_between()
            .items_start()
            .flex_wrap()
            .child(
                col()
                    .min_w(208.0)
                    .grow(1.0)
                    .gap(4.0)
                    .child(text(label).font(SMALL).color(if label == "Connecting" {
                        p.accent
                    } else {
                        p.green
                    }))
                    .child(text(title).font(SECTION).wrap_anywhere().selectable(true))
                    .child(description(format!("Session {}", short_id(id)))),
            )
            .child(actions),
    );
    if !sharing || session.bytes_sent != 0 || session.bytes_received != 0 {
        let path = session
            .path
            .as_deref()
            .unwrap_or("Waiting for a direct connection");
        panel = panel.child(line()).child(
            col()
                .gap(4.0)
                .child(text(path).font(LABEL).color(p.ink).wrap_anywhere())
                .child(description(format!(
                    "{} sent  ·  {} received",
                    bytes(session.bytes_sent),
                    bytes(session.bytes_received)
                ))),
        );
    }
    panel.children(
        session
            .invites
            .iter()
            .map(|invite| session_invite(id, invite, busy)),
    )
}

fn session_invite(share: &str, invite: &SessionInvite, busy: bool) -> El {
    let p = palette();
    let invite_id = &invite.id;
    let share_id = share.to_owned();
    let revoke_id = invite_id.clone();
    let mut item = col()
        .key(format!("invite-{invite_id}"))
        .gap(8.0)
        .px(12.0)
        .py(10.0)
        .radius(7.0)
        .border(1.0, p.border)
        .fill(p.field)
        .child(
            row()
                .gap(8.0)
                .justify_between()
                .center_y()
                .child(
                    col()
                        .gap(3.0)
                        .child(text(format!("Invite {}", short_id(invite_id))).font(LABEL))
                        .child(description(session_state(&invite.state))),
                )
                .child(button(
                    format!("revoke-{invite_id}"),
                    "Revoke",
                    ButtonStyle::Quiet,
                    busy || invite.state == "closed",
                    move |s, _| s.revoke(&share_id, &revoke_id),
                )),
        );
    item = item.children(invite.peers.iter().map(|peer| peer_view(share, peer, busy)));
    item
}

fn peer_view(share: &str, peer: &SessionPeer, busy: bool) -> El {
    let mut card = col()
        .key(format!("peer-{}", peer.id))
        .gap(6.0)
        .child(line())
        .child(
            row()
                .gap(10.0)
                .center_y()
                .justify_between()
                .child(text(format!("Peer {}", short_id(&peer.id))).font(LABEL))
                .child(description(session_state(&peer.state))),
        );
    if let Some(identity) = &peer.identity {
        card = card.child(
            text(format!("Device {identity}"))
                .font(CODE)
                .color(palette().muted)
                .wrap_anywhere()
                .min_w(0.0),
        );
    }
    if peer.state == "pending" {
        let (approve_share, approve_peer) = (share.to_owned(), peer.id.clone());
        let (remember_share, remember_peer) = (share.to_owned(), peer.id.clone());
        let (deny_share, deny_peer) = (share.to_owned(), peer.id.clone());
        let mut actions = row().gap(7.0).flex_wrap().child(button(
            format!("approve-{}", peer.id),
            "Approve",
            ButtonStyle::Primary,
            busy,
            move |s, _| s.approve(&approve_share, &approve_peer, false),
        ));
        if peer.identity.is_some() {
            actions = actions.child(button(
                format!("remember-{}", peer.id),
                "Approve and remember",
                ButtonStyle::Secondary,
                busy,
                move |s, _| s.approve(&remember_share, &remember_peer, true),
            ));
        }
        card = card.child(actions.child(button(
            format!("deny-{}", peer.id),
            "Decline",
            ButtonStyle::Danger,
            busy,
            move |s, _| s.deny(&deny_share, &deny_peer),
        )));
    }
    card
}

fn session_state(state: &str) -> &str {
    match state {
        "accepting" => "Accepting guests",
        "closed" => "Closed",
        "connected" => "Connected",
        "connecting" => "Connecting",
        "consumed" => "Used",
        "pending" => "Waiting for approval",
        _ => state,
    }
}

fn short_id(value: &str) -> String {
    value.chars().take(10).collect()
}

fn bytes(total: u64) -> String {
    if total < 1_024 {
        format!("{total} B")
    } else if total < 1_048_576 {
        format!("{:.1} KiB", total as f64 / 1_024.0)
    } else {
        format!("{:.1} MiB", total as f64 / 1_048_576.0)
    }
}

fn settings_view(s: &State) -> El {
    let network = card()
        .child(heading("Network", 2))
        .child(field(
            "settings-server",
            "Rendezvous server",
            "https://unbind.dev",
            &s.settings.server,
            FieldStyle::Line,
            |s| &mut s.settings.server,
        ))
        .child(field(
            "settings-stun",
            "STUN servers",
            "Comma-separated STUN URLs or none",
            &s.settings.stun,
            FieldStyle::Line,
            |s| &mut s.settings.stun,
        ))
        .child(
            row()
                .gap(11.0)
                .flex_wrap()
                .child(
                    field(
                        "settings-bind",
                        "Local UDP bind",
                        "0.0.0.0:40000",
                        &s.settings.udp_bind,
                        FieldStyle::Line,
                        |s| &mut s.settings.udp_bind,
                    )
                    .min_w(160.0)
                    .grow(1.0),
                )
                .child(
                    field(
                        "settings-public",
                        "Public UDP endpoint",
                        "198.51.100.2:40000",
                        &s.settings.public_endpoint,
                        FieldStyle::Line,
                        |s| &mut s.settings.public_endpoint,
                    )
                    .min_w(160.0)
                    .grow(1.0),
                )
                .child(
                    field(
                        "settings-pcp",
                        "PCP gateway",
                        "192.168.1.1",
                        &s.settings.pcp_gateway,
                        FieldStyle::Line,
                        |s| &mut s.settings.pcp_gateway,
                    )
                    .min_w(160.0)
                    .grow(1.0),
                ),
        )
        .child(description(
            "Leave optional UDP settings blank to use automatic address discovery.",
        ))
        .child(row().child(button(
            "settings-save",
            "Save settings",
            ButtonStyle::Primary,
            s.busy,
            |s, _| {
                s.save_settings();
            },
        )));
    let diagnostics = card()
        .child(heading("Connection diagnostics", 2))
        .child(description(
            "Check the rendezvous server and UDP address discovery on this network.",
        ))
        .child(row().child(button(
            "settings-diagnose",
            "Run diagnostics",
            ButtonStyle::Secondary,
            s.busy,
            |s, _| {
                s.diagnose();
            },
        )))
        .children(s.diagnostics.iter().map(|entry| {
            text(entry)
                .font(CODE)
                .color(palette().muted)
                .wrap_anywhere()
                .selectable(true)
                .min_w(0.0)
        }));
    col().gap(14.0).child(network).child(diagnostics).child(
        card()
            .child(heading("Desktop links", 2))
            .child(description(
                "Open Unbind invites in this app so you can review them before connecting.",
            ))
            .child(row().child(button(
                "settings-links",
                "Enable desktop links",
                ButtonStyle::Secondary,
                s.busy,
                |s, _| {
                    s.enable_links();
                },
            ))),
    )
}
