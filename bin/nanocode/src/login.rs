use nanocode::{
    claude::{ClaudeHomeLogin, subscription::ClaudeSubscriptionStatus},
    codex::auth::{ChatGptLogin, chatgpt_auth_file},
};
use nanocode_mpp::tempo_id::{self, DeviceLogin};
use nanocode_sync::mpsc::UnboundedSender;
use nanocode_tui::transcript::NoticeLevel;

use crate::app::{App, AppEvent};

impl App {
    pub(crate) fn login(&mut self, argument: &str) {
        let _span = nanocode_trace::info_span!(target: "nanocode", "nc.action", action.name = "login", session.id = self.agent.as_ref().map(|agent| agent.session_id())).entered();
        let tx = self.tx.clone();
        match argument.trim() {
            "codex" | "chatgpt" | "openai" => {
                nanocode_platform::spawn_task(codex(tx));
            }
            "claude" | "anthropic" => {
                nanocode_platform::spawn_task(claude(tx));
            }
            "tempo" => {
                nanocode_platform::spawn_task(tempo(tx));
            }
            _ => self.notice(NoticeLevel::Info, "usage: /login codex|claude|tempo"),
        }
    }

    pub(crate) fn fund(&mut self, argument: &str) {
        let _span = nanocode_trace::info_span!(target: "nanocode", "nc.action", action.name = "fund", session.id = self.agent.as_ref().map(|agent| agent.session_id())).entered();
        nanocode_platform::spawn_task(fund(
            self.tx.clone(),
            argument.trim().trim_start_matches('$').to_owned(),
        ));
    }

    pub(crate) fn pass(&mut self, argument: &str) {
        match self.host.config.clients.answering() {
            Some(answer) => {
                if let Some(reply) = answer(argument) {
                    self.notice(NoticeLevel::Info, format!("pass: {reply}"));
                }
            }
            None => self.notice(
                NoticeLevel::Info,
                "pass: no MPP service can ask for an email pass in this session",
            ),
        }
    }

    pub(crate) fn account(&mut self) {
        let _span = nanocode_trace::info_span!(target: "nanocode", "nc.action", action.name = "account", session.id = self.agent.as_ref().map(|agent| agent.session_id())).entered();
        nanocode_platform::spawn_task(account(self.tx.clone()));
    }
}

async fn codex(tx: UnboundedSender<AppEvent>) {
    let notice = |level, text| drop(tx.send(AppEvent::Notice(level, text)));
    let path = match chatgpt_auth_file() {
        Ok(path) => path,
        Err(error) => return notice(NoticeLevel::Error, format!("login codex: {error}")),
    };
    let login = match ChatGptLogin::start(path).await {
        Ok(login) => login,
        Err(error) => return notice(NoticeLevel::Error, format!("login codex: {error}")),
    };
    notice(
        NoticeLevel::Info,
        browse("ChatGPT sign-in", login.authorization_url()),
    );
    match login.complete().await {
        Ok(status) => notice(
            NoticeLevel::Info,
            format!(
                "signed in to ChatGPT as {}",
                status.email.unwrap_or(status.account_id)
            ),
        ),
        Err(error) => notice(NoticeLevel::Error, format!("login codex: {error}")),
    }
}

async fn claude(tx: UnboundedSender<AppEvent>) {
    let notice = |level, text| drop(tx.send(AppEvent::Notice(level, text)));
    let login = match ClaudeHomeLogin::start().await {
        Ok(login) => login,
        Err(error) => return notice(NoticeLevel::Error, format!("login claude: {error}")),
    };
    notice(
        NoticeLevel::Info,
        browse("Claude sign-in", login.authorization_url()),
    );
    match login.complete().await {
        Ok(ClaudeSubscriptionStatus::Authenticated { .. }) => {
            notice(NoticeLevel::Info, "signed in to Claude".to_owned());
        }
        Ok(status) => notice(
            NoticeLevel::Error,
            format!("login claude ended as {status:?}"),
        ),
        Err(error) => notice(NoticeLevel::Error, format!("login claude: {error}")),
    }
}

const TEMPO_PAYS: &str = "it pays for the Tempo gateway's and Mercator's tools and the Tempo services shell commands reach, and --gateway tempo pays for models with it; /fund buys MACH by card";

async fn tempo(tx: UnboundedSender<AppEvent>) {
    let notice = |level, text| drop(tx.send(AppEvent::Notice(level, text)));
    let login = match DeviceLogin::start(nanocode_mpp::tx::CHAIN_ID).await {
        Ok(login) => login,
        Err(error) => return notice(NoticeLevel::Error, format!("login tempo: {error}")),
    };
    notice(
        NoticeLevel::Info,
        format!(
            "{}; check that it shows {}",
            browse("Tempo ID sign-in", login.verification_url()),
            login.user_code()
        ),
    );
    match login.complete().await {
        Ok(account) => notice(
            NoticeLevel::Info,
            format!("signed in with Tempo ID as {account}; {TEMPO_PAYS}"),
        ),
        Err(error) => notice(NoticeLevel::Error, format!("login tempo: {error}")),
    }
}

async fn fund(tx: UnboundedSender<AppEvent>, amount: String) {
    let notice = |level, text| drop(tx.send(AppEvent::Notice(level, text)));
    let account = match nanocode_mpp::accounts::default_active_account() {
        Ok(account) => account,
        Err(error) => return notice(NoticeLevel::Error, format!("fund: {error}")),
    };
    let url = tempo_id::deposit_url(account, nanocode_mpp::tx::CHAIN_ID, Some(&amount));
    notice(
        NoticeLevel::Info,
        format!(
            "{}; MACH bought there goes to the Tempo account {account}, and nanocode pays with it once it arrives",
            browse("the Tempo ID deposit page", &url)
        ),
    );
}

async fn account(tx: UnboundedSender<AppEvent>) {
    let notice = |level, text| drop(tx.send(AppEvent::Notice(level, text)));
    let held = match nanocode_mpp::accounts::default_active_account() {
        Ok(account) => {
            format!("nanocode pays from the Tempo account {account}; /fund buys MACH for it")
        }
        Err(_) => "/login tempo signs nanocode in with Tempo ID".to_owned(),
    };
    notice(
        NoticeLevel::Info,
        format!("{}; {held}", browse("Tempo ID", tempo_id::TEMPO_ID_URL)),
    );
}

fn browse(page: &str, url: &str) -> String {
    if crate::extras::open_url(url) {
        format!("opened {page} in your browser; if it did not appear, visit {url}")
    } else {
        format!("visit {url} for {page}")
    }
}
