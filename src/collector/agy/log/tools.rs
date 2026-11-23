use crate::collector::ident::ident;

const CONFIRMED: &str = "ToolConfirmation";

/// The tool a `Responding to tool confirmation` message approved, or `None`
/// when it was declined. Only the granted list is read (`persistGrants`, or
/// `turnGrants` in older builds): searching the whole line for `command(`
/// counted a denied grant as a command run.
pub(in crate::collector::agy) fn confirmed_tool(message: &str) -> Option<String> {
    let (head, grants) = ["persistGrants=[", "turnGrants=["]
        .iter()
        .find_map(|marker| message.split_once(marker))
        .map_or((message, None), |(head, grants)| (head, Some(grants)));
    if head.contains("approved=false") {
        return None;
    }
    Some(
        grants
            .and_then(|grants| grants.strip_prefix("command("))
            .and_then(command_name)
            .unwrap_or_else(|| CONFIRMED.to_owned()),
    )
}

/// The tool an auto-approval message names: `<mode> mode: auto-approving
/// [pending ]file write "<Tool>" at step <n>`. Auto mode approves these
/// without a confirmation line, so they were not counted at all.
pub(in crate::collector::agy) fn auto_approved_tool(message: &str) -> Option<String> {
    let (mode, rest) = message.split_once(" mode: auto-approving ")?;
    if mode.is_empty() || mode.contains(char::is_whitespace) {
        return None;
    }
    let quoted = rest
        .strip_prefix("file write ")
        .or_else(|| rest.strip_prefix("pending file write "))?
        .strip_prefix('"')?;
    let (name, _) = quoted.split_once('"')?;
    base_name(name).map(ToOwned::to_owned)
}

/// `command:<basename>` of a grant's command line. Only the executable is
/// kept: the rest of the line is arguments, and an absolute executable path
/// published a home directory in the tool list.
fn command_name(command_line: &str) -> Option<String> {
    let token = command_line.split_whitespace().next()?;
    let executable = token.split(')').next()?;
    let executable = executable.trim_matches(|c| c == '"' || c == '\'');
    if executable.contains('=') {
        return None;
    }
    base_name(executable).map(|name| format!("command:{name}"))
}

fn base_name(name: &str) -> Option<&str> {
    let base = name.rsplit(['/', '\\']).next()?;
    (!base.trim().is_empty()).then_some(base).and_then(ident)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESPONDING: &str = "Responding to tool confirmation: convID=c1, stepIdx=1, approved=true, sandboxOverride=false, scope=PERMISSION_SCOPE_UNSPECIFIED, ";

    fn confirmed(grants: &str) -> Option<String> {
        confirmed_tool(&format!("{RESPONDING}{grants}"))
    }

    #[test]
    fn granted_command_names_its_executable() {
        assert_eq!(
            confirmed("persistGrants=[command(ls)], denyGrants=[]").as_deref(),
            Some("command:ls")
        );
        assert_eq!(
            confirmed(r#"persistGrants=[command(curl -sL https://example.test -w "%{http_code}\n")], denyGrants=[]"#)
                .as_deref(),
            Some("command:curl")
        );
        assert_eq!(
            confirmed("turnGrants=[command(bun run)]").as_deref(),
            Some("command:bun")
        );
    }

    #[test]
    fn executable_path_is_reduced_to_its_name() {
        for grants in [
            "persistGrants=[command(/home/someone/project/scripts/build)], denyGrants=[]",
            "persistGrants=[command(/home/someone/project/scripts/build --fast)], denyGrants=[]",
            r#"persistGrants=[command("C:\Users\someone\build")], denyGrants=[]"#,
        ] {
            assert_eq!(
                confirmed(grants).as_deref(),
                Some("command:build"),
                "{grants}"
            );
        }
    }

    #[test]
    fn denied_or_non_command_grants_name_no_command() {
        assert_eq!(
            confirmed("persistGrants=[], denyGrants=[command(rm)]").as_deref(),
            Some(CONFIRMED)
        );
        assert_eq!(
            confirmed("persistGrants=[read_file(/home/someone)], denyGrants=[]").as_deref(),
            Some(CONFIRMED)
        );
        assert_eq!(
            confirmed("persistGrants=[command(FOO=1 make)], denyGrants=[]").as_deref(),
            Some(CONFIRMED)
        );
        assert_eq!(
            confirmed_tool(
                "Responding to tool confirmation: convID=c1, stepIdx=1, approved=false, persistGrants=[command(rm)], denyGrants=[]"
            ),
            None
        );
        assert_eq!(
            confirmed("persistGrants=[command(echo approved=false)], denyGrants=[]").as_deref(),
            Some("command:echo")
        );
    }

    #[test]
    fn auto_approval_names_its_tool() {
        assert_eq!(
            auto_approved_tool(r#"auto mode: auto-approving file write "Edit" at step 2"#)
                .as_deref(),
            Some("Edit")
        );
        assert_eq!(
            auto_approved_tool(r#"auto mode: auto-approving pending file write "Write" at step 3"#)
                .as_deref(),
            Some("Write")
        );
        assert_eq!(
            auto_approved_tool(
                r#"auto mode: auto-approving file write "/home/someone/Edit" at step 1"#
            )
            .as_deref(),
            Some("Edit")
        );
        assert_eq!(
            auto_approved_tool(r#"auto mode: auto-approving file write "" at step 1"#),
            None
        );
        assert_eq!(
            auto_approved_tool(r#"said auto mode: auto-approving file write "Edit" at step 1"#),
            None
        );
    }

    #[test]
    fn no_emitted_name_carries_a_path_separator() {
        let names = [
            confirmed("persistGrants=[command(/a/b/c)], denyGrants=[]"),
            confirmed(r"persistGrants=[command(a\b\c)], denyGrants=[]"),
            confirmed("persistGrants=[command(/a/b/)], denyGrants=[]"),
            auto_approved_tool(r#"auto mode: auto-approving file write "a/b\c" at step 1"#),
            auto_approved_tool(r#"auto mode: auto-approving file write "a/" at step 1"#),
        ];
        for name in names.iter().flatten() {
            assert!(!name.contains(['/', '\\']), "{name}");
        }
    }
}
