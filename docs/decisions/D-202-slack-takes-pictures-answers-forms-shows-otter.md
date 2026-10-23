# D-202 — Slack takes pictures, answers forms, shows Otter writing, and has /otter (2026-10-23)
<!-- claim: a6f82e4af08cc07e609d0972edbc58dd0bfc1f65 -->

The developer asked for the rest of what D-200 and D-201 left undone, in
one change: pictures, a form of several questions, the answer shown as it
is written, and `/otter`. Each is its own commit. The Slack app needs
nothing new: its manifest already has `/otter`, `files:read` and
interactivity.

- **`/otter`.** Slack's Socket Mode sends a slash command as an envelope
  whose ack can carry the answer (`accepts_response_payload`), seen only by
  who typed it, and only a listed member is answered. `/otter` and
  `/otter status` say what needs the developer, longest waiting first, and
  what is at work, newest first. `/otter new <words>` makes a task of the
  words and starts it. The title is their first line. Otter places it from
  them (D-186). It works in a thread of its own in the developer's direct
  messages: the task's message is posted first, and the task is made with
  `command_id` `slack:<dm>:<ts>:<ts>`. That makes its request a message
  "written in that thread", so D-200's rule sends what follows it there,
  and a reply there goes to the task. Anything else gets what `/otter`
  does. The thread is recorded by the sync loop, which alone writes
  `state/slack.json`: the command hands it over in memory, and a reply that
  comes first finds it there.
- **A form.** A decision asked as a form — several questions, or one whose
  choices may be several (its interaction, D-059) — is asked as the app
  asks it. Each question gets its own block: its choices in a select
  (several where it takes several), a box where it has none, then Submit.
  Submit takes each question's answer from two places: what the message
  holds (`state.values`), and what each select chose as it was chosen.
  Slack's documents don't say plainly that a message's `state` comes with
  a press, so both are read. It sends `decide {answers}`, several choices
  joined with ", " as the app joins them. A question left unanswered is
  said in the thread, and nothing is decided. A single question with one
  choice keeps D-201's buttons, and one without choices keeps "reply to
  answer".
- **Pictures.** A picture sent with a message (a `file_share`) is fetched
  with the bot's token: the file while it fits D-171's 700 KB, else Slack's
  thumbnails, largest first. Its type is read from its first bytes (png,
  jpeg, webp), not from what Slack says it is. At most four, 2 MB in all.
  A file that isn't one, or doesn't fit, is said to be left out. The
  message goes where it would have, with its pictures: to Otter on the
  host, or the task under whose message it was written. `otter.send` takes
  `images` as `task.send` does. They are kept beside the conversation and
  shown to Otter's turn on the host (D-181's rule), which saw none before.
  Words with pictures under a question are said, not taken as the answer.
- **Otter's answer as it is written.** Otter's turn already streams its
  answer (`OtterStream`, `TaskStream` from Otter) as `reply-<the
  developer's message>`, and the answer lands with that message as its
  `correlation_id`. Where that message came from Slack, the first words are
  posted there ("_Otter is writing…_" before any), then shown again as
  more come, at most every 1.5 s (`chat.update` is rate-limited). When the
  sync sends the answer, it takes that message's place, rest posted after
  if long. It never comes as a second message. The sync waits up to 3 s
  for first words still being posted, and words that come after the answer
  are dropped. Only Otter's answers: the coding agent's words along the way
  aren't the message it ends with (D-069).

**Not done:** the coding agent's words as they are written, a form's
answers kept across a restart of otterd (what the message holds is read at
Submit anyway), pictures in Otter's answers back to Slack, `/otter` in a
channel the app isn't in beyond its own answer, several hosts from one
app.

Tests: unit — `/otter`'s words, an interactive envelope, a form's answers
from a message and a press's pick, a form drawn question by question, a
message's pictures best first and their type by their bytes, `status` and
titles. End to end, against the stand-in Slack:
- `otter_in_slack_says_what_needs_you_and_starts_a_task`: `status` lists a
  waiting task, someone not listed is refused, the help, and `new` makes
  and starts a task whose request is in its thread, Otter in that thread,
  a reply there in the task.
- `a_form_is_answered_in_slack`: a pick as it is chosen, a Submit with a
  question unanswered says so, then one with the other in the message's
  state answers both.
- `a_picture_in_slack_reaches_otter`: the thumbnail fetched, not the
  picture too large, with the bot's token; Otter's turn shown it; a PDF
  said to be left out.
- `a_direct_message_in_slack_is_answered_there` (and D-201's) now see the
  answer shown as it is written, then in its place, one message.

Watched failing: without the message's state, Submit never answers; with
the host turn shown no pictures, Otter never sees it; with the answer
always posted anew, it doesn't take the words' place. Checked against
Slack's documented payloads, not yet against Slack itself.
