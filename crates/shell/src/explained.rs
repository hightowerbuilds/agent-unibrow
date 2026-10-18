//! The "Explained" sidebar tab: a short guide to getting the most out of
//! the agent. Example requests are clickable and drop into the chat box.

use crate::theme;
use gpui::{AnyElement, ClickEvent, IntoElement, ParentElement, SharedString, Styled, Window, div, prelude::*, px, rgb};

enum Block {
    Para(&'static str),
    Bullets(&'static [&'static str]),
    /// Requests the user can click to put in the chat box.
    Examples(&'static [&'static str]),
    /// (keys, what they do)
    Keys(&'static [(&'static str, &'static str)]),
}

struct Section {
    title: &'static str,
    blocks: &'static [Block],
}

const GUIDE: &[Section] = &[
    Section {
        title: "What the agent is",
        blocks: &[
            Block::Para(
                "The agent works in this browser, on the page you're looking at. It reads pages, clicks, types, \
                 opens tabs, downloads files, and checks your Gmail, and you watch every step. You stay in charge: \
                 anything that buys, sends, posts or deletes waits for your OK.",
            ),
            Block::Para(
                "Pick which agent runs it at the top of Chat. Claude Code and Codex use your own subscriptions; \
                 API key bills an Anthropic API key; OpenCode uses whatever provider you've signed it into.",
            ),
        ],
    },
    Section {
        title: "Asking well",
        blocks: &[
            Block::Bullets(&[
                "Say what you want done, not how to click: \"find the cheapest TUSQ jazz bass nut\" beats \"click the search box\".",
                "Name the site when it matters, or put sites in Focus so it prefers them.",
                "Give the details it can't guess: sizes, quantities, budget, dates, which account.",
                "Say \"draft\" or \"don't send\" when you only want it prepared. It prefers drafts anyway.",
                "One errand per message works best. Follow up in the same chat; it remembers the conversation.",
                "If it's already on the right page, just say \"on this page…\".",
            ]),
            Block::Para("Try one (click to put it in the chat box):"),
            Block::Examples(&[
                "Find a TUSQ XL jazz bass nut on Sweetwater and tell me the price and whether it's in stock.",
                "Compare the first three results for this search in separate tabs and give me a short table.",
                "Read this page and summarize it in five bullet points.",
                "Find the three emails from this week that need a reply and draft responses. Don't send them.",
                "What have I been looking at on my focus sites today?",
                "Download the receipt on this page and tell me the order total.",
            ]),
        ],
    },
    Section {
        title: "Staying in control",
        blocks: &[
            Block::Bullets(&[
                "Approval cards: buying, paying, sending, posting, deleting, submitting forms that send information, \
                 uploading a file, and confirming risky pop-ups all ask first, with a screenshot and the values.",
                "\"Always allow\" on a card stops asking for that site (or those email recipients).",
                "Stop it any time: the Stop button on the page, Esc, or just click or type on the page yourself.",
                "The highlight box shows what it's about to click or type into.",
                "Trace lists every step it took, with a screenshot for each browser action. Click a row for details.",
            ]),
        ],
    },
    Section {
        title: "What it won't do",
        blocks: &[
            Block::Bullets(&[
                "Type passwords, card numbers, security codes or one-time codes. It hands those to you, and can't see \
                 them on the page or in screenshots.",
                "Solve CAPTCHAs or get around sites that block agents. It tells you and stops.",
                "Upload files from hidden folders like ~/.ssh or ~/.config.",
                "Allow camera, microphone, location or notifications for sites.",
            ]),
            Block::Para(
                "When it needs you (a sign-in, a code, a CAPTCHA), it says so and waits. Do that part, then tell it \
                 to carry on.",
            ),
        ],
    },
    Section {
        title: "History, bookmarks and focus",
        blocks: &[
            Block::Bullets(&[
                "History keeps every page you open here, and the agent can search it (\"that bass part I looked at yesterday\").",
                "☆ in the toolbar bookmarks a page; open them from the Bookmarks dropdown in History.",
                "◎ Focus marks a site as one you care about. The agent sees your focus sites with every message and \
                 prefers them.",
            ]),
        ],
    },
    Section {
        title: "Files",
        blocks: &[
            Block::Bullets(&[
                "Downloads save to ~/Downloads/agent-unibrow. The ↓ button in the toolbar lists them.",
                "The agent can read downloaded text, CSV and PDF files, and save any page as a PDF.",
                "To upload, tell it the file's full path. It asks before attaching anything.",
            ]),
        ],
    },
    Section {
        title: "Shortcuts",
        blocks: &[Block::Keys(&[
            ("Ctrl+B", "Show or hide the sidebar"),
            ("Alt+1 … Alt+5", "Chat, History, Focus, Trace, Explained"),
            ("Esc", "Stop the agent"),
            ("Ctrl+T / Ctrl+W", "New tab / close tab"),
            ("Ctrl+Tab", "Next tab (Shift for previous)"),
            ("Ctrl+L", "Jump to the address bar"),
            ("Ctrl+F", "Find in page"),
        ])],
    },
];

/// Renders the guide. `on_example` puts a request in the chat box.
pub fn render(on_example: impl Fn(&'static str, &mut Window, &mut gpui::App) + Clone + 'static) -> impl IntoElement {
    let mut col = div().id("explained").flex().flex_col().flex_1().min_h(px(0.)).overflow_y_scroll().px_4().py_3().gap_4();
    col = col.child(
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_base().text_color(rgb(theme::FG)).child("Unibrow, explained"))
            .child(div().text_xs().text_color(rgb(theme::MUTED)).child("How to get the most out of the agent.")),
    );
    let mut example_ix = 0usize;
    for section in GUIDE {
        let mut sec = div().flex().flex_col().gap_2().child(
            div().text_xs().text_color(rgb(theme::ACCENT)).child(section.title.to_uppercase()),
        );
        for block in section.blocks {
            let el: AnyElement = match block {
                Block::Para(t) => div().text_sm().text_color(rgb(theme::FG)).line_height(px(20.)).child(*t).into_any_element(),
                Block::Bullets(items) => div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(items.iter().map(|t| {
                        div()
                            .flex()
                            .gap_2()
                            .text_sm()
                            .line_height(px(20.))
                            .child(div().text_color(rgb(theme::ACCENT)).child("•"))
                            .child(div().flex_1().min_w(px(0.)).text_color(rgb(theme::FG)).child(*t))
                    }))
                    .into_any_element(),
                Block::Examples(items) => div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(items.iter().map(|t| {
                        example_ix += 1;
                        let on_example = on_example.clone();
                        let text: &'static str = t;
                        div()
                            .id(("example", example_ix))
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(theme::RULE))
                            .bg(rgb(theme::BG))
                            .text_sm()
                            .text_color(rgb(theme::FG))
                            .cursor_pointer()
                            .hover(|s| s.border_color(rgb(theme::ACCENT)))
                            .on_click(move |_: &ClickEvent, window, cx| on_example(text, window, cx))
                            .child(SharedString::from(*t))
                    }))
                    .into_any_element(),
                Block::Keys(rows) => div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(rows.iter().map(|(k, what)| {
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .text_sm()
                            .child(
                                div()
                                    .w(px(130.))
                                    .flex_none()
                                    .child(
                                        div()
                                            .px_2()
                                            .py(px(1.))
                                            .rounded_sm()
                                            .border_1()
                                            .border_color(rgb(theme::RULE))
                                            .bg(rgb(theme::BG))
                                            .text_xs()
                                            .text_color(rgb(theme::FG))
                                            .child(*k),
                                    ),
                            )
                            .child(div().text_color(rgb(theme::MUTED)).child(*what))
                    }))
                    .into_any_element(),
            };
            sec = sec.child(el);
        }
        col = col.child(sec);
    }
    col
}
