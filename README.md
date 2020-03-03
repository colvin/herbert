# herbert

A simple actor framework in Rust built around centralized message routers.

`herbert` supports a program architecture pattern consisting of well-defined
and long-lived threads that maintain their own state and interact with one
another through messages sent over channels.

The problem with this approach is keeping track of all of the channels,
propagating them amongst arbitrary threads, and rationalizing about their
lifecycles. `herbert` addresses these challenges using a centralized message
router. The router is responsible for spawning actors upon request, monitoring
and responding to changes in their state, and routing messages to and between
them.
