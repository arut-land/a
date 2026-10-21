# Approval explanations

`Permission::Ask(ApprovalReason)` carries the rule that requires approval.
The reason travels with the permission decision in the log, so reconnects,
snapshots and replay explain the same question. Rule kinds distinguish a
session mode, an operator rule (including its input match), a path outside
the workspace, and a classifier's reason. Extensions supply these facts;
clients never infer them from tool names or inputs. Equal permission ranks
retain the first handler's reason, independently of its explanatory data.

`client::approval_text` supplies the wording for the TUI, web, line REPL,
pipe-mode exit-3 diagnostic, and the native app through FFI. The shared
view associates the reason with its tool and an optional completed side
reply with that same owner. It reports both changed items together, so a
description refreshes an already-visible question in every client.

Set `[ext.policy] describe = true` to add a plain-language description.
It is off by default because it spends a model call. Policy observes the
logged ask and starts a background `Fast` request with no conversation
context and a 96-token cap. The question never waits for that request.
Descriptions are labelled `What (model)` separately from the typed `Why`;
a missing or failed description leaves the question usable.

The SDK's `Background::while_waiting` scopes optional work to its owner's
question. The driver checks that scope both when opening and admitting a
side call. An answer cancels the side token without cancelling the owner's
execution token. If the answer races admission, the cancelled request
records interruption without polling the provider. Existing interruption,
shutdown, replay and durable side-result rules continue to apply. No
approval-specific state or effect is added to the kernel.
