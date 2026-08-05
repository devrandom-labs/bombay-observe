# observepass

Generation-safe completion publication and asynchronous observation. The
initial mutex implementation is a semantic and performance baseline only.

Actorpass will map an actor generation to a subject, publish its outcome on task
completion, and translate observations into pure `ChildStopped` or
`PeerStopped` events. Observepass owns none of that actor vocabulary.

Required races:

```text
observe -> complete  => observer receives outcome
complete -> observe  => late observer receives retained outcome
retire(old)           => cannot remove replacement
drop observer         => cannot obstruct completion
```

Run `.auto/checks.sh`; use `.auto/prompt.md` with OMP `/autoresearch`.

