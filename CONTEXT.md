# CONTEXT — kith

> **kith** — local-first, peer-to-peer collections shared with the people you trust.
> v0.1 wedge: share wallpapers with your friends. No cloud, no account, no server.

This file is the **glossary**. It defines the ubiquitous language of the product and
nothing else — no implementation detail, no specification, no scratch notes. Specs live
in `docs/spec/`, locked technical decisions in `docs/adr/`.

---

## People

### Person

A human being. The unit of trust and the unit of social identity. A Person is *not* an
account — there is no registry, no server, no login. A Person exists because other People
have chosen to trust them.

A Person owns one or more **Devices**. Membership, attribution and permission are always
expressed in terms of the Person, never the Device.

> Prefer **Person** over "user" and "account" everywhere. "User" implies a system that
> issues identities; kith issues none.

### Device

One installation of kith on one machine. A Device is the thing that actually connects,
syncs and stores bytes. Every Device belongs to exactly one Person.

A Person with a laptop and a desktop has one identity and two Devices. Content added on
either is attributed to the Person, not the Device.

### Identity

The cryptographic material that lets a Device prove it speaks for a Person. Local to the
Device; never escrowed, never recoverable from anywhere else. Losing every Device that
holds a Person's Identity means losing that Identity — kith has no recovery authority
because it has no authority at all.

---

## Groups

### Circle

A named group of People who have chosen to share with each other. The Circle is the
boundary of trust and the boundary of sync: content inside a Circle is visible to every
Member of that Circle and to nobody else.

A Circle is the product's central social object. **People over devices** — a Circle is a
group of People, even though the transport underneath connects Devices.

### Member

A Person's participation in a Circle. Carries a **Role**. The same Person may be a Member
of many Circles, with a different Role in each.

### Membership claim

The record in which one Device states, inside one Circle, which Person it speaks for and
under what name. It is keyed by the **Device** and written by that Device alone — one file,
one writer — and it is never deleted, so an old Device's contributions keep resolving to a
named Person long after the machine itself has gone.

The Membership claim is the bridge between a Person and their Devices, and the basis of all
attribution: every fact kith states about who did what keys on the Person named *inside* the
claim, never on the Device that wrote it. A Person with two Devices is two claims naming one
Person, which is why a second Device costs nothing to model.

> **A claim, not a proof.** Nothing signs it. A claim is believable because a human admitted
> that Device to the Circle, not because kith can verify what it says — see `docs/adr/`.

### Unclaimed Device

A Device that is in a Circle but has published no Membership claim of its own. kith can see
that it holds the Circle's content and cannot say whose it is, so it shows the Device by its
fingerprint and names no Person.

> **Never hidden.** A Device receiving a Circle's content is a fact the Circle is entitled to
> see, and an unnamed one most of all.

### Role

A Member's declared capability within a Circle — for example who may invite, who may
remove content, who may rename the Circle.

> **A Role is a policy, not an enforcement.** kith has no server to arbitrate, and the
> transport underneath synchronises bytes to every Member equally. A Role describes what
> a well-behaved kith client will do and what the Circle has agreed to; it cannot stop a
> determined Member from writing to files their Device already holds. Every part of the
> product that mentions Roles must be honest about this — see `docs/adr/`.

### Steward

The Member whose Device is a Circle's only way in: every join is approved there, and no other
Member is asked. A Steward is a **Person** — where a passage genuinely means the machine, it
says *the Steward's Device*.

*v0.1:* one Steward per Circle, and they are its one admin. A Circle whose Steward's Device
is unreachable keeps syncing and can admit nobody until it returns.

### Invite

A time-bounded offer to join a Circle, issued by a Member whose Role permits it. An
Invite is the *only* way into a Circle; there is no discovery, no directory, no public
Circle.

An Invite is consumed by **joining**, or it expires, or it is revoked.

### Invite window

The period during which a Circle is expecting someone to knock. Issuing an Invite opens the
window, and the window is kept on the Steward's Device because that is the one machine where
admission actually happens. The Invite itself carries the same bound as a courtesy, so an
invitee's kith can refuse a stale one without asking anybody.

> **The window closes; the Invite is not taken back.** An Invite is a pointer to a Device,
> not a credential — whoever has read one can knock again. Expiry ends the period in which a
> knock is expected; it un-publishes nothing, and approval remains a deliberate human act.

---

## Content

### Collection

A named set of **Items** living inside a Circle. A Collection is a *logical* space, not a
directory — "Collections over folders". It has its own metadata, its own Provider, and
its own membership of Items independent of how those Items are laid out on disk.

*v0.1:* every Circle has exactly one Collection, created with the Circle. The one-to-many
Circle-to-Collection relationship is modelled from the start so it can be opened up later
without a migration.

### Item

One piece of content in a Collection — the bytes plus everything kith knows about them:
who added it, when, its title, its tags, whether it is a favourite.

An Item is not a file. A file is how an Item's bytes happen to be stored; the Item is the
domain object, and it survives being moved, renamed, or re-encoded.

### Adopted Item

An Item kith found rather than one a Person handed it — bytes that were already sitting in a
Circle when kith arrived, or that a peer not running kith dropped into one. kith records them
so that nothing in a Circle is invisible, and dates them from the bytes themselves rather
than from the moment of discovery.

> **Found, not added.** An adopted Item reads *found by Ana*, never *added by Ana*. Being the
> first Device to notice something is a weaker claim than having added it, and the surface
> makes the weaker claim.

### Sidecar

The synced record that carries an Item's metadata alongside its bytes. Sidecars exist
because the transport moves bytes and knows nothing about meaning — every fact that is
*about* content rather than *in* content lives in a Sidecar.

Sidecars are written by many Devices at once with no coordinator, so their format is
constrained by conflict tolerance rather than convenience. See `docs/adr/`.

### Favourite

A Person's private mark on an Item. Favourites are per-Person, not per-Circle: marking an
Item does not announce anything to the Circle.

---

## Behaviour

### Provider

The content-type-aware layer. A Provider teaches kith how to deal with one kind of
content: how to **preview** it, what **Actions** it offers, what **metadata** to read from
it, and how to import and export it.

The core knows about People, Circles, Collections and Items. It knows nothing about
wallpapers, videos or documents — that knowledge is entirely a Provider's.

*v0.1:* one Provider, the **wallpaper provider**.

### Action

An operation a Provider offers on an Item — apply, reveal, open, copy path, favourite,
delete. Actions are how a Collection stops being a folder and starts being a workflow.

### Apply

The Action that makes an Item *active* on this Device. For the wallpaper Provider, Apply
sets the Item as the desktop background, optionally on a chosen monitor.

Apply is always local and always deliberate: content arriving from a Circle never changes
what is on a Person's screen without that Person having asked for it.

### Sync Engine

The replaceable transport seam. The Sync Engine's whole job is to make a Circle's bytes
present on every Member Device; it has no opinion about Collections, Items or Providers.

*v0.1:* Syncthing, driven over its REST API as a separately-running daemon.

> **Replace implementations, not concepts.** "Syncthing" must not appear in any domain
> conversation — the concept is the Sync Engine. Syncthing-specific vocabulary (folder,
> device ID, introducer) belongs behind the seam, in `docs/adr/` and the sync-engine spec.

### Presence

Whether this Device is holding an open connection to another Person's Device right now.
Presence is pairwise and live: it is one Device's own view, worked out when it is asked, and
never written into anything the Circle shares.

> **Presence, never "online".** "Online" implies a human at a keyboard; a connection implies
> a socket, and a socket is all kith has. A Member this Device cannot reach may be connected
> to another Member, and *unknown* is a real answer rather than a polite way of saying no.

### Activity

The record of what has happened in a Circle — Items added, Members joined, content
removed. Derived from what has synced, not from a log anyone is authoritative about.

---

## Words we deliberately do not use

| Avoid | Use instead | Why |
|---|---|---|
| User, account | **Person** | kith issues no identities |
| Folder, directory | **Collection** | Collections are logical; directories are storage |
| File | **Item** | An Item is the domain object; a file is where its bytes sit |
| Friend | **Member** | Membership is of a Circle and carries a Role |
| Permission, ACL | **Role** | Nothing is enforced; be honest |
| Roster | **Members**, or **Membership claims** | One names the People, the other the records; "roster" blurs which is meant |
| Online, offline, last seen | **Presence** | Presence is one Device's live view of a connection, never a claim about a Person |
| Syncthing | **Sync Engine** | The transport is replaceable |
| Server, cloud, sync service | — | There is none. Say so plainly. |
