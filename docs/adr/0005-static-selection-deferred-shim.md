# MVP selection is a static substituter list, not a discovery shim

Choosing between box and remote cache could be done with an mDNS-discovered local
proxy from day one, but that's meaningful new client-side surface (a service, plus a
discovery backend) for a problem the MVP can absorb more cheaply: a static substituter
list (`[box, remote cache]`) with a low `connect-timeout` gets the on-network speedup
immediately, at the cost of a small timeout tax when off-network. We're deferring the
shim until that tax is actually felt, rather than building the polish before the
substance (the eager replica) is proven.

This also sidesteps an environment constraint: the box currently runs as a container
inside a k3s cluster, and mDNS (link-local, TTL=1 multicast) cannot cross the
cluster's CNI overlay to reach the host LAN without extra plumbing. A future shim will
support pluggable discovery backends — static-endpoint (works anywhere, including
inside k8s) and mDNS (zero-config, for bare-metal/LAN-host deployments) — so this
constraint doesn't box in the OSS packaging goal.
