//! The users (profiles) on one phone, and which apps are installed in more than
//! one of them.
//!
//! A phone can hold several users at once: the owner (0), a work profile, a
//! clone/dual-app profile, a guest. Each has its own copy of the package list,
//! and `pm list packages` on its own answers for the user the shell happens to
//! be in — so an app that exists in a work profile looks like it is not
//! installed at all, and an app that exists in both looks like one app.
//!
//! That matters to a launch: when the same app is installed twice, Android
//! asks which copy to open instead of opening one, and a caller that does not
//! know the app has a second copy reads the question as a failure.
//!
//! This module is the pure half — reading `pm list users` / `pm list packages`
//! and working out which packages are shared — so the rules can be tested
//! without a device. The two reads themselves live beside it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use super::android::{element_label, UiElement};

/// One user on the phone, as `pm list users` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserProfile {
    /// The user id: `0` is the owner, and anything else is another profile.
    pub id: i32,
    /// What the profile is called (`机主`, `工作资料`, `Guest`), possibly empty.
    pub name: String,
    /// Whether the profile is running. A stopped user has no process and no
    /// apps of its own in memory, but its package list is still real.
    pub running: bool,
}

impl UserProfile {
    /// What to call this profile in a result: its own name, or its id when it
    /// has none.
    pub fn label(&self) -> String {
        let name = self.name.trim();
        if name.is_empty() {
            format!("user {}", self.id)
        } else {
            name.to_string()
        }
    }
}

/// The user the owner's own screen belongs to. A phone whose *current* user
/// cannot be read is described against this one, because it is the only id
/// every Android release agrees on.
pub const OWNER: i32 = 0;

/// One `UserInfo{<id>:<name>:<hex>} [running]` line.
///
/// The name is everything between the first and second colon; a name with a
/// colon in it is not something the platform produces, and splitting on the
/// last colon would turn a name ending in a hex-looking word into an id.
fn user_line() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    #[allow(clippy::unwrap_used)]
    RE.get_or_init(|| {
        regex::Regex::new(r"UserInfo\{(\d+):([^:}]*):[0-9a-fA-F]+\}(\s+running)?").unwrap()
    })
}

/// Read `pm list users`.
///
/// Lines that are not a `UserInfo{…}` entry (the `Users:` header, a warning)
/// are skipped rather than refused: the command is the authority on which
/// users exist, and a build that prefixes its output with something else is
/// still saying the same thing.
pub fn parse_users(output: &str) -> Vec<UserProfile> {
    output
        .lines()
        .filter_map(|line| {
            let found = user_line().captures(line)?;
            let id = found.get(1)?.as_str().parse::<i32>().ok()?;
            Some(UserProfile {
                id,
                name: found
                    .get(2)
                    .map(|m| m.as_str().trim().to_string())
                    .unwrap_or_default(),
                running: found.get(3).is_some(),
            })
        })
        .collect()
}

/// Read one `pm list packages` output.
///
/// A line is `package:<name>`; with `-U` it is `package:<name> uid:<n>`, and
/// only the name is wanted. Everything else the command may print — a warning,
/// a blank line — is skipped.
pub fn parse_packages(output: &str) -> BTreeSet<String> {
    output
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("package:")?;
            let name = rest.split_whitespace().next()?;
            if name.is_empty() {
                None
            } else {
                Some(name.to_string())
            }
        })
        .collect()
}

/// Which packages each other profile also has installed: package → profile
/// names, in the order the profiles were given.
///
/// Only *other* profiles belong here — a package being in the list the caller
/// already has is not news. A profile with no name is reported by its id, so
/// the answer can always be said out loud.
pub fn also_in(others: &[(UserProfile, BTreeSet<String>)]) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (profile, packages) in others {
        let name = profile.label();
        for package in packages {
            let names = out.entry(package.clone()).or_default();
            if !names.contains(&name) {
                names.push(name.clone());
            }
        }
    }
    out
}

/// Which copy of an app one option in a "which app?" chooser is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileCopy {
    /// The copy in the profile the caller is already working in.
    Main,
    /// The copy in another profile, named as that profile is.
    Other(String),
    /// Which copy it is cannot be told from the label alone.
    Unmarked,
}

impl ProfileCopy {
    /// The word a result carries.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Other(_) => "other_profile",
            Self::Unmarked => "unmarked",
        }
    }

    /// The sentence that goes beside the option, when there is one to add.
    pub fn note(&self) -> Option<String> {
        match self {
            Self::Main => Some("the copy in the profile the phone is on".to_string()),
            Self::Other(profile) => Some(format!("the copy in {profile}")),
            Self::Unmarked => None,
        }
    }
}

/// Run one command as the shell identity and return its output.
///
/// The shell is the only identity that can list other users and their
/// packages (`pm list packages --user <id>` answers `Error: no packages found`
/// to anyone else). It is the identity the display helper already runs as, so
/// this goes through the same loopback-ADB pairing.
///
/// `None` is any failure at all — no shell, a refusal, a non-zero exit. The
/// callers all treat "could not ask" as a different answer from "the answer is
/// empty", which is why this is an `Option` and not a string that happens to
/// be blank.
pub async fn shell(device: &Option<String>, command: &[&str]) -> Option<String> {
    let mut argv: Vec<String> = Vec::with_capacity(command.len() + 3);
    if let Some(serial) = device {
        argv.push("-s".to_string());
        argv.push(serial.clone());
    }
    argv.push("shell".to_string());
    argv.extend(command.iter().map(|part| part.to_string()));
    let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    let (status, stdout, _) = super::run_cmd("adb", &argv_refs).await.ok()?;
    if !status.success() {
        return None;
    }
    Some(stdout)
}

/// The users on the phone, or `None` when it could not be asked.
pub async fn users(device: &Option<String>) -> Option<Vec<UserProfile>> {
    shell(device, &["pm", "list", "users"])
        .await
        .map(|output| parse_users(&output))
}

/// The packages installed in every profile other than `current`.
///
/// `None` means the question could not be asked at all — no shell, or a `pm`
/// that does not accept `--user`. A profile whose list could not be read is
/// omitted rather than reported as empty: "it has none" is a claim about the
/// profile, and one failed read does not establish it.
pub async fn other_profiles(
    device: &Option<String>,
    current: i32,
) -> Option<Vec<(UserProfile, BTreeSet<String>)>> {
    let users = users(device).await?;
    let others: Vec<UserProfile> = users
        .into_iter()
        .filter(|user| user.id != current)
        .collect();
    if others.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::with_capacity(others.len());
    for profile in others {
        let id = profile.id.to_string();
        // A profile whose list would not read is left out: reporting it as
        // empty would say "it has no apps", which one failed read does not
        // establish.
        if let Some(output) = shell(device, &["pm", "list", "packages", "--user", &id]).await {
            out.push((profile, parse_packages(&output)));
        }
    }
    Some(out)
}

/// The user whose screen the phone is showing, or `None` when it could not be
/// asked.
///
/// This is what "the phone's own copy" means in a chooser: the copy in the
/// profile the person is actually in, which is user 0 only until somebody
/// signs into a second profile and leaves it switched on.
pub async fn current_user(device: &Option<String>) -> Option<i32> {
    let output = shell(device, &["am", "get-current-user"]).await?;
    output.trim().parse::<i32>().ok()
}

/// One option of a "which app?" chooser, as it is on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// The label exactly as the screen shows it — what `android_dialog` needs.
    pub label: String,
    /// Which copy of the app it is.
    pub copy: ProfileCopy,
    /// Where to press to pick it.
    pub center: (i32, i32),
}

/// A label reduced to what survives decoration: case, spacing, and the
/// parentheses or dash a vendor puts around a profile's name all go.
fn bare(label: &str) -> String {
    label
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Read the options of a "which app?" chooser from the tree on screen.
///
/// The chooser's title ("Open with", "选择要打开的应用") is not what identifies
/// it: it is in the platform's language, and no list of phrases covers every
/// vendor build. What identifies it is the shape — a system window whose
/// clickable options are one app's name, some of them naming a profile the app
/// is installed in a second time.
///
/// The copy from the caller's own profile carries the bare name and the other
/// carries the profile's name on top of it, so a profile-named option tells
/// the caller what the bare name is. That is how the bare option is told apart
/// from the dialog's own buttons: "取消" and "仅此一次" are not the app's name,
/// and are left out of the answer rather than offered as a way to open it.
///
/// `None` means this screen is not a chooser. An empty tree, a dialog with one
/// button, and a screen whose options name no profile all come back as
/// `None`: each of those is a different question, and reading one as a chooser
/// would have the caller press a button that is not answering it.
pub fn chooser_options(elements: &[UiElement], profiles: &[UserProfile]) -> Option<Vec<Choice>> {
    // Every label is read, not just the clickable ones: the copy from another
    // profile is sometimes a plain row of text inside a clickable container,
    // and the container carries no name. What is offered as an answer is
    // decided below. A label long enough to be a paragraph is the dialog's
    // message rather than one of its options.
    let labels: Vec<(String, bool, (i32, i32))> = elements
        .iter()
        .filter_map(|element| {
            element_label(element).map(|label| {
                (label.replace('\n', " ").trim().to_string(), element.clickable, element.center)
            })
        })
        .filter(|(label, _, _)| !label.is_empty() && label.chars().count() <= 40)
        .collect();
    // The question needs at least two answers, and the title is not one.
    if labels.iter().filter(|(_, clickable, _)| *clickable).count() < 2 {
        return None;
    }

    let named_profiles: Vec<&UserProfile> = profiles
        .iter()
        .filter(|profile| !bare(&profile.name).is_empty())
        .collect();

    // The bare name of the app, recovered from each profile-named option by
    // taking the profile's own name back out of it.
    let mut base_names: Vec<String> = Vec::new();
    let mut seen_profile = false;
    let mut options: Vec<(String, ProfileCopy, (i32, i32))> = Vec::new();
    for (label, _, center) in &labels {
        let bare_label = bare(label);
        let profile = named_profiles
            .iter()
            .find(|profile| bare_label.contains(&bare(&profile.name)));
        match profile {
            Some(profile) => {
                seen_profile = true;
                let base = bare_label.replace(&bare(&profile.name), "");
                if !base.is_empty() && !base_names.contains(&base) {
                    base_names.push(base);
                }
                options.push((label.clone(), ProfileCopy::Other(profile.label()), *center));
            }
            None => options.push((label.clone(), ProfileCopy::Unmarked, *center)),
        }
    }
    if !seen_profile || base_names.is_empty() {
        return None;
    }

    // A bare option is the caller's own copy when its name is one of the
    // recovered base names. When two options share that name, the screen is
    // telling them apart by a badge rather than by text, and neither is more
    // "the main one" than the other — both stay unmarked.
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for (label, _, _) in &labels {
        *counts.entry(bare(label)).or_default() += 1;
    }
    let choices: Vec<Choice> = options
        .into_iter()
        .filter_map(|(label, copy, center)| match copy {
            ProfileCopy::Other(profile) => Some(Choice {
                label,
                copy: ProfileCopy::Other(profile),
                center,
            }),
            // Options are only ever `Other` or `Unmarked` before this point;
            // `Main` here would be a copy whose profile was not named, which
            // is exactly what `Unmarked` means.
            ProfileCopy::Main | ProfileCopy::Unmarked => {
                let bare_label = bare(&label);
                let clickable = labels
                    .iter()
                    .find(|(text, _, _)| *text == label)
                    .is_some_and(|(_, clickable, _)| *clickable);
                if !clickable || !base_names.contains(&bare_label) {
                    return None;
                }
                let copy = if counts.get(&bare_label).copied().unwrap_or(0) == 1 {
                    ProfileCopy::Main
                } else {
                    ProfileCopy::Unmarked
                };
                Some(Choice { label, copy, center })
            }
        })
        .collect();
    if choices.len() < 2 {
        return None;
    }
    Some(choices)
}

/// The sentence a launch's result carries when the system asked which copy.
pub fn describe_choices(choices: &[Choice]) -> String {
    let options = choices
        .iter()
        .map(|choice| match choice.copy.note() {
            Some(note) => format!("{:?} ({note})", choice.label),
            None => format!("{:?}", choice.label),
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "The app is installed more than once, so the system is asking which copy to open: \
         {options}. Pick one with android_dialog `button` set to exactly that label — or pick \
         the copy the task needs. Until one is picked, the display is showing the chooser, not \
         the app."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::platform::mobile::android::UiElement;

    fn button(label: &str) -> UiElement {
        let mut element = UiElement::new(
            1,
            Some(label.to_string()),
            None,
            None,
            "android.widget.TextView".to_string(),
            true,
            true,
            (0, 0, 300, 100),
        );
        element.clickable = true;
        element
    }

    fn profile(name: &str) -> UserProfile {
        UserProfile {
            id: 10,
            name: name.to_string(),
            running: true,
        }
    }

    #[test]
    fn a_user_line_names_the_profile_and_whether_it_is_running() {
        let users = parse_users(
            "Users:\n\tUserInfo{0:机主:c13} running\n\tUserInfo{128:工作资料:1030} running\n\t\
             UserInfo{10:Guest:404}\n",
        );
        assert_eq!(
            users,
            vec![
                UserProfile {
                    id: 0,
                    name: "机主".to_string(),
                    running: true
                },
                UserProfile {
                    id: 128,
                    name: "工作资料".to_string(),
                    running: true
                },
                UserProfile {
                    id: 10,
                    name: "Guest".to_string(),
                    running: false
                },
            ]
        );
    }

    #[test]
    fn a_profile_with_no_name_is_still_nameable() {
        let users = parse_users("\tUserInfo{11::a1} running\n");
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].label(), "user 11");
        // Whitespace is not a name either.
        assert_eq!(
            UserProfile {
                id: 7,
                name: "  ".to_string(),
                running: false
            }
            .label(),
            "user 7"
        );
    }

    #[test]
    fn lines_that_are_not_a_user_are_skipped() {
        // The header, a warning, and a malformed entry: none of them is a user.
        let users = parse_users("Users:\nWARNING: Could not parse\nUserInfo{x:y:z}\n");
        assert!(users.is_empty());
    }

    #[test]
    fn a_package_line_is_the_name_and_nothing_else() {
        let packages = parse_packages(
            "package:com.example.chat\npackage:com.example.pay uid:10123\r\n\nWARNING: x\n",
        );
        assert_eq!(
            packages,
            BTreeSet::from([
                "com.example.chat".to_string(),
                "com.example.pay".to_string(),
            ])
        );
    }

    #[test]
    fn a_shared_app_says_which_profiles_have_it() {
        let work = UserProfile {
            id: 10,
            name: "工作资料".to_string(),
            running: true,
        };
        let clone = UserProfile {
            id: 999,
            name: String::new(),
            running: true,
        };
        let shared = also_in(&[
            (work, BTreeSet::from(["com.example.chat".to_string()])),
            (
                clone,
                BTreeSet::from([
                    "com.example.chat".to_string(),
                    "com.example.mail".to_string(),
                ]),
            ),
        ]);
        assert_eq!(
            shared.get("com.example.chat").cloned().unwrap(),
            vec!["工作资料".to_string(), "user 999".to_string()]
        );
        assert_eq!(shared.get("com.example.mail").cloned().unwrap(), vec!["user 999".to_string()]);
        // A package only the caller's own profile has is not in the answer.
        assert!(!shared.contains_key("com.example.notes"));
    }

    #[test]
    fn a_chooser_offers_the_two_copies_and_says_which_is_which() {
        let screen = vec![
            button("选择要打开的应用"),
            button("Chat"),
            button("Chat（工作资料）"),
            button("取消"),
        ];
        let choices = chooser_options(&screen, &[profile("工作资料")]).expect("a chooser");
        assert_eq!(
            choices
                .iter()
                .map(|choice| (choice.label.as_str(), choice.copy.clone()))
                .collect::<Vec<_>>(),
            vec![
                ("Chat", ProfileCopy::Main),
                ("Chat（工作资料）", ProfileCopy::Other("工作资料".to_string())),
            ],
            "the dialog's own buttons are not copies of the app"
        );
        let said = describe_choices(&choices);
        assert!(said.contains("more than once"), "{said}");
        assert!(said.contains("android_dialog"), "{said}");
        // Each option carries where to press it, so the caller can press
        // without a second read of the screen.
        assert_eq!(choices[0].center, (150, 50));
    }

    #[test]
    fn two_copies_with_the_same_label_are_not_guessed_between() {
        // A vendor that marks the clone with a badge rather than by name: two
        // rows say "Chat", and which one is the phone's own cannot be read —
        // so neither is called the main copy and the caller has to choose.
        let screen = vec![button("Chat"), button("Chat"), button("Chat（工作资料）")];
        let choices = chooser_options(&screen, &[profile("工作资料")]).expect("a chooser");
        assert_eq!(choices.len(), 3);
        assert_eq!(
            choices
                .iter()
                .filter(|choice| choice.copy == ProfileCopy::Unmarked)
                .count(),
            2
        );
        assert!(choices
            .iter()
            .all(|choice| choice.copy != ProfileCopy::Main));
    }

    #[test]
    fn identical_options_with_no_profile_name_are_not_a_chooser() {
        // Nothing on the screen says which profile it is about, so this could
        // be any dialog with two identical buttons. Guessing would have the
        // caller press one.
        let screen = vec![button("Chat"), button("Chat")];
        assert_eq!(chooser_options(&screen, &[profile("工作资料")]), None);
    }

    #[test]
    fn a_dialog_that_names_no_profile_is_not_asking_which_copy() {
        // A permission prompt mentions the app and has buttons; it is not a
        // chooser, and opening a copy is not one of its answers.
        let screen = vec![
            button("Allow Chat to send notifications?"),
            button("Allow"),
            button("Don't allow"),
        ];
        assert_eq!(chooser_options(&screen, &[profile("工作资料")]), None);
        // A chooser with one option is not a question.
        assert_eq!(chooser_options(&[button("Chat（工作资料）")], &[profile("工作资料")]), None);
        // An empty screen is not a chooser either.
        assert_eq!(chooser_options(&[], &[profile("工作资料")]), None);
    }

    #[test]
    fn a_profile_named_with_decoration_is_still_recognised() {
        // The badge spelling varies by vendor: parentheses, a dash, a line
        // break, full-width brackets.
        for label in ["Chat-工作资料", "Chat\n工作资料", "Chat【工作资料】"] {
            let screen = vec![button("Chat"), button(label)];
            let choices = chooser_options(&screen, &[profile("工作资料")]).expect(label);
            assert_eq!(choices[0].copy, ProfileCopy::Main, "{label}");
            assert_eq!(choices[1].copy, ProfileCopy::Other("工作资料".to_string()), "{label}");
        }
    }
}
