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

### Role

A Member's declared capability within a Circle — for example who may invite, who may
remove content, who may rename the Circle.

> **A Role is a policy, not an enforcement.** kith has no server to arbitrate, and the
> transport underneath synchronises bytes to every Member equally. A Role describes what
> a well-behaved kith client will do and what the Circle has agreed to; it cannot stop a
> determined Member from writing to files their Device already holds. Every part of the
> product that mentions Roles must be honest about this — see `docs/adr/`.

### Invite

A time-bounded offer to join a Circle, issued by a Member whose Role permits it. An
Invite is the *only* way into a Circle; there is no discovery, no directory, no public
Circle.

An Invite is consumed by **joining**, or it expires, or it is revoked.

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
| Syncthing | **Sync Engine** | The transport is replaceable |
| Server, cloud, sync service | — | There is none. Say so plainly. |
