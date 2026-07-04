# Box originates no content and holds no signing key

Every path served or mirrored by the box was already signed upstream (by CI or the
laptop, with the existing `znix.zebradil.dev` key) before it reached the box, so the
box never needs to sign anything itself — it stores and passes signatures through
unchanged. The box does need remote-cache read/write credentials to mirror
bidirectionally, but deliberately does *not* hold the private signing key: it's the
most exposed, always-on component, so a compromise there can at worst desync the
mirror, never forge a trusted store path.

Considered: box has its own signing key, laptop pushes unsigned and box signs on
mirror-up (rejected — moves the private key onto the highest-exposure host for no
benefit, since the laptop can sign directly).
