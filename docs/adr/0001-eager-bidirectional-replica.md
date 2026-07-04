# Box is an eager bidirectional replica, not a pull-through proxy

Each network (home, later work) has exactly one machine pulling from the cache and it
retains what it already built, so a lazy pull-through box would still pay the full
remote-cache latency on every first request — no different from reading the remote
cache directly. Only an always-on box that eagerly mirrors new generations down from
the remote cache *before* anyone asks, and mirrors local pushes up in the background,
actually moves the slow remote hop off the interactive path. We chose eager
bidirectional replication over tools built around lazy pull-through (e.g. `ncps`).

Considered: read-only pull-through replica (rejected — no read speedup with a single
LAN consumer, and no accelerated reverse-flow push).
