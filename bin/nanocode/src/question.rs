use std::{collections::BTreeMap, sync::Arc};

use nanocode::tools::{
    Tool, ToolContext, ToolDefinition, ToolInput, ToolOutput, ToolResult, contract::ToolCallFuture,
    runtime::DynamicToolProvider,
};
use nanocode_sync::{Atom, mpsc::UnboundedSender, oneshot};
use serde::Deserialize;

pub const NAME: &str = "AskUserQuestion";

#[derive(Clone, Debug, Deserialize)]
pub struct Choice {
    pub label: String,
    pub description: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    pub question: String,
    pub header: String,
    pub options: Vec<Choice>,
    #[serde(default)]
    pub multi_select: bool,
}

#[derive(Deserialize)]
struct Input {
    questions: Vec<Question>,
}

pub struct Asked {
    pub questions: Vec<Question>,
    pub answers: oneshot::Sender<Option<Vec<Answer>>>,
}

#[derive(Clone, Debug)]
pub enum Answer {
    Chosen(Vec<String>),
    Typed(String),
}

#[derive(Clone)]
pub struct Questions {
    sink: Arc<Atom<Option<UnboundedSender<Asked>>>>,
}

impl Questions {
    pub fn new() -> Self {
        Self {
            sink: Arc::new(Atom::new(None)),
        }
    }

    pub fn answer_with(&self, sink: UnboundedSender<Asked>) {
        self.sink.set(Some(sink));
    }
}

struct AskTool {
    sink: Arc<Atom<Option<UnboundedSender<Asked>>>>,
}

fn rendered(questions: &[Question], answers: &[Answer]) -> String {
    let pairs = questions
        .iter()
        .zip(answers)
        .map(|(question, answer)| {
            let text = match answer {
                Answer::Chosen(labels) => labels.join(", "),
                Answer::Typed(text) => text.clone(),
            };
            format!("\"{}\"=\"{text}\"", question.question)
        })
        .collect::<Vec<_>>()
        .join(", ");
    match answers
        .iter()
        .all(|answer| matches!(answer, Answer::Chosen(_)))
    {
        true => format!(
            "Your questions have been answered: {pairs}. You can now continue with these answers in mind."
        ),
        false => format!(
            "The user answered: {pairs}. Read the answers carefully \u{2014} they may request clarification, changes, or that you not proceed \u{2014} and follow what they actually say."
        ),
    }
}

impl Tool for AskTool {
    fn definition(&self) -> ToolDefinition {
        let upstream = nanocode::claude::tool_contracts::tool(NAME);
        ToolDefinition::function(NAME, upstream.description.clone(), upstream.input_schema())
    }

    fn execute<'life0, 'life1, 'async_trait>(
        &'life0 self,
        input: ToolInput,
        _context: ToolContext<'life1>,
    ) -> ToolCallFuture<'async_trait, ToolResult>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let Input { questions } = input.decode_json::<Input>()?;
            if questions.is_empty() {
                return Ok(ToolOutput::error("ask at least one question"));
            }
            if let Some(question) = questions.iter().find(|question| question.options.len() < 2) {
                return Ok(ToolOutput::error(format!(
                    "question \"{}\" needs at least two options",
                    question.question
                )));
            }
            let Some(sink) = self.sink.read(Option::clone) else {
                return Ok(ToolOutput::error(
                    "nobody can answer questions in this session",
                ));
            };
            let (answers, answered) = oneshot::channel();
            if sink
                .send(Asked {
                    questions: questions.clone(),
                    answers,
                })
                .is_err()
            {
                return Ok(ToolOutput::text("The user did not answer the questions."));
            }
            Ok(ToolOutput::text(match answered.await {
                Ok(Some(answers)) if answers.len() == questions.len() => {
                    rendered(&questions, &answers)
                }
                Ok(_) | Err(_) => "The user did not answer the questions.".to_owned(),
            }))
        })
    }
}

impl DynamicToolProvider for Questions {
    fn direct_tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(AskTool {
            sink: Arc::clone(&self.sink),
        })]
    }
}

pub struct Pending {
    pub questions: Vec<Question>,
    pub answers: Vec<Answer>,
    pub picked: BTreeMap<usize, bool>,
    pub cursor: usize,
    pub reply: oneshot::Sender<Option<Vec<Answer>>>,
}

impl crate::app::App {
    pub(crate) fn asked(&mut self, asked: Asked) {
        self.questions_waiting.push_back(asked);
        self.next_questions();
    }

    fn next_questions(&mut self) {
        if self.questioning.is_some() {
            return;
        }
        let Some(Asked { questions, answers }) = self.questions_waiting.pop_front() else {
            return;
        };
        self.questioning = Some(Pending {
            questions,
            answers: Vec::new(),
            picked: BTreeMap::new(),
            cursor: 0,
            reply: answers,
        });
        self.open_question();
    }

    pub(crate) fn asking(&self) -> Option<&Question> {
        let pending = self.questioning.as_ref()?;
        pending.questions.get(pending.answers.len())
    }

    fn open_question(&mut self) {
        let Some(pending) = &self.questioning else {
            return;
        };
        let Some(question) = pending.questions.get(pending.answers.len()) else {
            return;
        };
        let mut items = question
            .options
            .iter()
            .enumerate()
            .map(|(index, choice)| {
                let item = nanocode_tui::picker::PickerItem::choice(choice.label.clone())
                    .detail(choice.description.clone())
                    .current(index == pending.cursor);
                match pending.picked.get(&index).copied().unwrap_or(false) {
                    true => item.badge("✓"),
                    false => item,
                }
            })
            .collect::<Vec<_>>();
        items.push(
            nanocode_tui::picker::PickerItem::choice("Other")
                .detail("type your own answer in the prompt"),
        );
        if question.multi_select {
            items.push(
                nanocode_tui::picker::PickerItem::choice("Done")
                    .detail("answer with the options marked ✓"),
            );
        }
        let position = match pending.questions.len() {
            1 => String::new(),
            count => format!(" · question {} of {count}", pending.answers.len() + 1),
        };
        let picker = nanocode_tui::picker::Picker::new(
            format!("{}: {}", question.header, question.question),
            items,
        )
        .footer(match question.multi_select {
            true => format!("enter marks an option · Done answers · esc declines{position}"),
            false => format!("enter answers · esc declines{position}"),
        });
        self.overlay = Some(crate::app::Overlay::Picker(
            crate::app::PickerPurpose::Question,
            picker,
        ));
    }

    pub(crate) fn question_picked(&mut self, index: usize) {
        let Some(pending) = &mut self.questioning else {
            return;
        };
        let Some(question) = pending.questions.get(pending.answers.len()) else {
            return;
        };
        let options = question.options.len();
        match (index, question.multi_select) {
            (index, false) if index < options => {
                let label = question.options[index].label.clone();
                pending.answers.push(Answer::Chosen(vec![label]));
            }
            (index, true) if index < options => {
                let picked = pending.picked.entry(index).or_insert(false);
                *picked = !*picked;
                pending.cursor = index;
            }
            (index, _) if index == options => {
                self.typing_answer = true;
                self.flash("type your answer, then press enter");
                return;
            }
            (_, true) => {
                let labels = question
                    .options
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| pending.picked.get(index).copied().unwrap_or(false))
                    .map(|(_, choice)| choice.label.clone())
                    .collect::<Vec<_>>();
                if labels.is_empty() {
                    self.flash("mark at least one option, or choose Other");
                } else {
                    pending.answers.push(Answer::Chosen(labels));
                    pending.picked.clear();
                    pending.cursor = 0;
                }
            }
            (_, false) => return,
        }
        self.after_answer();
    }

    pub(crate) fn typed_answer(&mut self, text: String) {
        self.typing_answer = false;
        if let Some(pending) = &mut self.questioning {
            pending.answers.push(Answer::Typed(text));
            pending.picked.clear();
            pending.cursor = 0;
        }
        self.after_answer();
    }

    fn after_answer(&mut self) {
        let done = self
            .questioning
            .as_ref()
            .is_some_and(|pending| pending.answers.len() == pending.questions.len());
        if done {
            if let Some(pending) = self.questioning.take() {
                let _ = pending.reply.send(Some(pending.answers));
            }
            self.next_questions();
        } else {
            self.open_question();
        }
    }

    pub(crate) fn decline_questions(&mut self) {
        self.typing_answer = false;
        if let Some(pending) = self.questioning.take() {
            let _ = pending.reply.send(None);
        }
        self.next_questions();
    }
}
