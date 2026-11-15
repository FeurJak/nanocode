use crate::deadline::Reminders;

pub(crate) fn notice(deadline: Option<&Reminders>) -> String {
    let time = deadline.map_or_else(String::new, |reminders| format!(" {}", reminders.remain()));
    format!(
        "<system-reminder>\nThis session is non-interactive and nobody will reply.{time} Before you finish, check the work:\n- Every output the task names exists at that exact path, with a relative path resolved against the working directory.\n- The task's own check, or a direct test of the result, passes when you run it again now.\n- Scratch files you created are removed wherever the task's outputs or checks may read them.\n- Files your own test runs wrote where the task's program or its checks write output or wait for it, such as a file in /tmp, are deleted, and processes you started are stopped.\n- Any choice left open is made, and its result is written.\nFix what falls short. When everything holds, reply with your final message.\n</system-reminder>"
    )
}
