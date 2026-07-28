# Telegram package

This package bridges KAS Threads to topics in one Telegram forum group.

Each `/manifests/telegram` Resource configures one bot and one group. Creating a
managed `thread-topic` Link asks the Driver to create a new Telegram topic for
that KAS Thread. The Driver stores Telegram's returned topic ID on the Link.
Incoming Telegram messages become KAS Message Resources, and KAS Messages in a
bound Thread are sent to the corresponding topic.

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
