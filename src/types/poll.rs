use crate::types::{MessageEntity, PollType};
use serde::{Deserialize, Serialize};

/// This object contains information about a poll.
///
/// [The official docs](https://core.telegram.org/bots/api#poll).
#[serde_with_macros::skip_serializing_none]
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct Poll {
    /// Unique poll identifier.
    pub id: String,

    /// Poll question, 1-255 characters.
    pub question: String,

    /// List of poll options.
    pub options: Vec<PollOption>,

    /// `true`, if the poll is closed.
    pub is_closed: bool,

    /// Total number of users that voted in the poll
    pub total_voter_count: i32,

    /// True, if the poll is anonymous
    pub is_anonymous: bool,

    /// Poll type, currently can be “regular” or “quiz”
    #[serde(rename = "type")]
    pub poll_type: PollType,

    /// True, if the poll allows multiple answers
    pub allows_multiple_answers: bool,

    /// 0-based identifier of the correct answer option. Available only for
    /// polls in the quiz mode, which are closed, or was sent (not
    /// forwarded) by the bot or to the private chat with the bot.
    pub correct_option_id: Option<i32>,

    /// Text that is shown when a user chooses an incorrect answer or taps on
    /// the lamp icon in a quiz-style poll, 0-200 characters.
    pub explanation: Option<String>,

    /// Special entities like usernames, URLs, bot commands, etc. that appear in
    /// the explanation.
    pub explanation_entities: Option<Vec<MessageEntity>>,

    /// Amount of time in seconds the poll will be active after creation.
    open_period: Option<i32>,

    /// Point in time (Unix timestamp) when the poll will be automatically
    /// closed.
    close_date: Option<i32>,
}

impl Poll {
    #[allow(clippy::too_many_arguments)]
    pub fn new<S1, S2, O>(
        id: S1,
        question: S2,
        options: O,
        is_closed: bool,
        total_voter_count: i32,
        is_anonymous: bool,
        poll_type: PollType,
        allows_multiple_answers: bool,
    ) -> Self
    where
        S1: Into<String>,
        S2: Into<String>,
        O: Into<Vec<PollOption>>,
    {
        Self {
            id: id.into(),
            question: question.into(),
            options: options.into(),
            is_closed,
            total_voter_count,
            is_anonymous,
            poll_type,
            allows_multiple_answers,
            correct_option_id: None,
            explanation: None,
            explanation_entities: None,
            open_period: None,
            close_date: None,
        }
    }

    pub fn id<S>(mut self, val: S) -> Self
    where
        S: Into<String>,
    {
        self.id = val.into();
        self
    }

    pub fn question<S>(mut self, val: S) -> Self
    where
        S: Into<String>,
    {
        self.question = val.into();
        self
    }

    pub fn options<P>(mut self, val: P) -> Self
    where
        P: Into<Vec<PollOption>>,
    {
        self.options = val.into();
        self
    }

    #[allow(clippy::wrong_self_convention)]
    pub fn is_closed(mut self, val: bool) -> Self {
        self.is_closed = val;
        self
    }

    pub fn total_voter_count(mut self, val: i32) -> Self {
        self.total_voter_count = val;
        self
    }

    #[allow(clippy::wrong_self_convention)]
    pub fn is_anonymous(mut self, val: bool) -> Self {
        self.is_anonymous = val;
        self
    }

    pub fn poll_type(mut self, val: PollType) -> Self {
        self.poll_type = val;
        self
    }

    pub fn allows_multiple_answers(mut self, val: bool) -> Self {
        self.allows_multiple_answers = val;
        self
    }

    pub fn correct_option_id(mut self, val: i32) -> Self {
        self.correct_option_id = Some(val);
        self
    }

    pub fn explanation<S>(mut self, val: S) -> Self
    where
        S: Into<String>,
    {
        self.explanation = Some(val.into());
        self
    }

    pub fn explanation_entities<S>(mut self, val: S) -> Self
    where
        S: Into<Vec<MessageEntity>>,
    {
        self.explanation_entities = Some(val.into());
        self
    }

    pub fn open_period(mut self, val: i32) -> Self {
        self.open_period = Some(val);
        self
    }

    pub fn close_date(mut self, val: i32) -> Self {
        self.close_date = Some(val);
        self
    }
}

/// This object contains information about one answer option in a poll.
///
/// [The official docs](https://core.telegram.org/bots/api#polloption).
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct PollOption {
    /// Option text, 1-100 characters.
    pub text: String,

    /// Number of users that voted for this option.
    pub voter_count: i32,
}

impl PollOption {
    pub fn new<S>(text: S, voter_count: i32) -> Self
    where
        S: Into<String>,
    {
        Self { text: text.into(), voter_count }
    }

    pub fn text<S>(mut self, val: S) -> Self
    where
        S: Into<String>,
    {
        self.text = val.into();
        self
    }

    pub fn voter_count(mut self, val: i32) -> Self {
        self.voter_count = val;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize() {
        let data = r#"
        {
            "allows_multiple_answers": false,
            "id": "5377643193141559299",
            "is_anonymous": true,
            "is_closed": false,
            "options": [
                {
                    "text": "1",
                    "voter_count": 1
                },
                {
                    "text": "2",
                    "voter_count": 0
                },
                {
                    "text": "3",
                    "voter_count": 0
                },
                {
                    "text": "4",
                    "voter_count": 0
                },
                {
                    "text": "5",
                    "voter_count": 0
                }
            ],
            "question": "Rate me from 1 to 5.",
            "total_voter_count": 1,
            "type": "regular"
        }
        "#;
        serde_json::from_str::<Poll>(data).unwrap();
    }
}
