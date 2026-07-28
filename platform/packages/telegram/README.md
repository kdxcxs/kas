# Telegram package

This package bridges KAS Threads to topics in one Telegram forum group.

Each `/manifests/telegram` Resource configures one bot and one group. Creating a
managed `thread-topic` Link asks the Driver to create a new Telegram topic for
that KAS Thread. The Driver stores Telegram's returned topic ID on the Link.
Incoming Telegram messages become KAS Message Resources, and KAS Messages in a
bound Thread are sent to the corresponding topic.

## User binding

The platform UI can generate a short-lived, single-use Telegram binding link
for the current KAS User. Opening the link starts a private conversation with
the configured bot. The Driver verifies the token and creates a `user-binding`
Link between that KAS User and the Telegram identity.

After binding:

- incoming Telegram Messages are attributed to the bound KAS User instead of
  the fallback `/users/telegram/{id}` identity;
- pending Approval requests are delivered to the user's private Telegram chat;
- the binding can be removed from the platform UI without deleting either
  identity.

The bot username is discovered with `getMe` and stored on the Telegram
configuration Resource so the UI can construct the binding link.

## Approvals

Pending Approval requests are sent as private messages with **Approve** and
**Reject** buttons. When a bound user presses a button, the Driver verifies the
callback against its delivery Link, issues a two-minute Credential for the
mapped KAS User, submits the decision to the Approval API, and immediately
revokes the Credential. The Approval API remains responsible for checking that
the user has permission to perform the requested operation.

The Telegram message is updated with the final outcome and its buttons are
removed. Approval messages are not pinned.

## Managed topics

Only topics created through these managed Links participate in synchronization.
Pre-existing topics and the General topic are deliberately ignored. Renaming a
Thread renames its Telegram topic; removing the Link closes the topic without
deleting its message history.

To let ordinary group messages reach KAS without mentioning the bot, make the
bot a group administrator or disable its privacy mode with BotFather. Users
still mention KAS Agents explicitly (for example, `@reviewer`) to trigger them.
The bot also needs Telegram's `can_manage_topics` administrator permission.

Configuration fields:

- `bot_token`: token issued by BotFather.
- `chat_id`: numeric Telegram supergroup ID, usually beginning with `-100`.
- `mode`: `bidirectional`, `telegram-to-kas`, or `kas-to-telegram`.
- `api_base`: optional Bot API base URL; omit it for Telegram's hosted API.
- `bot_username`: discovered by the Driver; it should not normally be entered
  manually.
