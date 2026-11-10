# leiser-harness

An agent harness for one developer's daily work: it runs coding agents on
the developer's own subscriptions and is extended through plugins.

## Language

**Provider**:
A company that offers models, such as Anthropic or OpenAI.
_Avoid_: vendor, lab, backend

**Subscription**:
One way the harness is allowed to use a provider's models: a plan signed in
by login, such as "Claude Max private" or "Claude Team work", or an API key.
A provider can have several.
_Avoid_: abo, account, plan, credential

**Git host**:
A service that hosts repositories, such as GitHub or GitLab.
_Avoid_: remote, forge, platform

**Host account**:
One user account on a git host, such as a private and a business GitHub
account. Every repository is bound to exactly one host account.
_Avoid_: GitHub account, profile, login, identity
