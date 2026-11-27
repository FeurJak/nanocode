pub fn serve_if_requested() -> Option<std::process::ExitCode> {
    nanocode_psh::serve_if_requested(|arguments| psh::run_with(arguments, &[]))
        .or_else(nanocode_process::read_outputs_if_requested)
}

pub fn install() -> std::io::Result<()> {
    let shell = nanocode_psh::shell()?;
    nanocode_home::shell::choose(shell).map_err(|chosen| {
        std::io::Error::other(format!(
            "this process chose its shell already: {}",
            chosen.path().display()
        ))
    })?;
    nanocode_psh_embedded::embed()
        .map_err(|_| std::io::Error::other("this process embedded a shell already"))
}
