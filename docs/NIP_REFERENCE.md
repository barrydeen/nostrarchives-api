# NIP Knowledge Base for nostr-api

_Generated automatically on 2026-03-12 00:34 UTC from the upstream nips repository._

This document catalogs every published Nostr Improvement Proposal (NIP) so the nostr-api team has one place to read the intent, status, and implementation hooks for each spec before touching ingestion code. Each entry lists upstream status flags, a short summary extracted from the canonical NIP, high-value section headings to dig deeper, and concrete notes on how the feature intersects with our ingestion/stats surface.

## Reading Guide
- **Status flags** come directly from each NIP (`final`, `draft`, `optional`, `mandatory`, `relay`, etc.).
- **Summary** is the first descriptive paragraph from the upstream spec; read the linked file for full detail.
- **Key mechanics** echoes the top section headings so you know what the spec covers without scrolling the entire file.
- **Event kinds / objects** lists the numeric kinds explicitly called out. Empty means the NIP focuses on process/policy rather than a new kind.
- **Implementation focus** highlights what nostr-api must do (storage semantics, relaying behavior, auth, analytics). Treat these as requirements when building features that touch the relevant data.

## Reference by NIP number

### NIP-01 — Basic protocol flow description
- **Source file:** `01.md`
- **Status:** draft · mandatory · relay
- **Summary:** ```yaml { "id": <32-bytes lowercase hex-encoded sha256 of the serialized event data>, "pubkey": <32-bytes lowercase hex-encoded public key of the event creator>, "created_at": <unix timestamp in seconds>, "kind": <integer between 0 and 65535>, "tags": [ [<arbitrary string>...], // ... ], "content": <arbitrary string>, "sig": <64-bytes lowercase hex of the signature of the sha256 hash of the serialized event data, which is the same as the "id" field> } ```
- **Key mechanics:** Events and signatures, Communication between clients and relays
- **Event kinds / objects:** 1, 10002
- **Implementation focus (nostr-api):** Store them as replaceable list-style events; keep only the latest per pubkey/kind so stats stay accurate.

### NIP-02 — Follow List
- **Source file:** `02.md`
- **Status:** final · optional
- **Summary:** ```jsonc { "kind": 3, "tags": [ ["p", "91cf9..4e5ca", "wss://alicerelay.com/", "alice"], ["p", "14aeb..8dad4", "wss://bobrelay.com/nostr", "bob"], ["p", "612ae..e610f", "ws://carolrelay.com/ws", "carol"] ], "content": "", // other fields... } ```
- **Key mechanics:** Uses
- **Event kinds / objects:** 3
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec. Document this behavior in our relay-connection guide so ingestion daemons negotiate capabilities and auth correctly.

### NIP-03 — OpenTimestamps Attestations for Events
- **Source file:** `03.md`
- **Status:** draft · optional
- **Summary:** ```json { "kind": 1040 "tags": [ ["e", <target-event-id>, <relay-url>], ["k", "<target-event-kind>"] ], "content": <base64-encoded OTS file data> } ```
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** 1040
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec. Document this behavior in our relay-connection guide so ingestion daemons negotiate capabilities and auth correctly.

### NIP-04 — Encrypted Direct Message
- **Source file:** `04.md`
- **Status:** final · unrecommended · optional · relay
- **Upstream note:** **unrecommended**: deprecated in favor of [NIP-17](17.md)
- **Summary:** ```js import crypto from 'crypto' import * as secp from '@noble/secp256k1'
- **Key mechanics:** Security Warning, Client Implementation Warning
- **Event kinds / objects:** 4
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec.

### NIP-05 — Mapping Nostr keys to DNS-based internet identifiers
- **Source file:** `05.md`
- **Status:** final · optional
- **Summary:** ```jsonc { "pubkey": "b0635d6a9851d3aed0cd6c495b282167acf761729078d975fc341b22650b07b9", "kind": 0, "content": "{\"name\": \"bob\", \"nip05\": \"bob@example.com\"}" // other fields... } ```
- **Key mechanics:** Finding users from their NIP-05 identifier, Notes
- **Event kinds / objects:** 0
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec.

### NIP-06 — Basic key derivation from mnemonic seed phrase
- **Source file:** `06.md`
- **Status:** draft · optional
- **Summary:** mnemonic: leader monkey parrot ring guide accident before fence cannon height naive bean\ private key (hex): 7f7ff03d123792d6ac594bfa67bf6d0c0ab55b6b1fdb6249303fe861f1ccba9a\ nsec: nsec10allq0gjx7fddtzef0ax00mdps9t2kmtrldkyjfs8l5xruwvh2dq0lhhkp\ public key (hex): 17162c921dc4d2518f9a101db33695df1afb56ab82f5ff3e5da6eec3ca5cd917\ npub: npub1zutzeysacnf9rru6zqwmxd54mud0k44tst6l70ja5mhv8jjumytsd2x7nu
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Primarily client-facing semantics; keep this documented so future features interpret these events/tags consistently.

### NIP-07 — `window.nostr` capability for web browsers
- **Source file:** `07.md`
- **Status:** draft · optional
- **Summary:** ``` async window.nostr.getPublicKey(): string // returns a public key as hex async window.nostr.signEvent(event: { created_at: number, kind: number, tags: string[][], content: string }): Event // takes an event object, adds `id`, `pubkey` and `sig` and returns it ```
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Primarily client-facing semantics; keep this documented so future features interpret these events/tags consistently.

### NIP-08 — Handling Mentions
- **Source file:** `08.md`
- **Status:** final · unrecommended · optional
- **Upstream note:** **unrecommended**: deprecated in favor of [NIP-27](27.md)
- **Summary:** (Upstream spec does not include a narrative summary.)
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Primarily client-facing semantics; keep this documented so future features interpret these events/tags consistently.

### NIP-09 — Event Deletion Request
- **Source file:** `09.md`
- **Status:** draft · optional · relay
- **Summary:** ```jsonc { "kind": 5, "pubkey": <32-bytes hex-encoded public key of the event creator>, "tags": [ ["e", "dcd59..464a2"], ["e", "968c5..ad7a4"], ["a", "<kind>:<pubkey>:<d-identifier>"], ["k", "1"], ["k", "30023"] ], "content": "these posts were published by accident", // other fields... } ```
- **Key mechanics:** Client Usage, Relay Usage, Deletion Request of a Deletion Request
- **Event kinds / objects:** 5
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec.

### NIP-10 — Text Notes and Threads
- **Source file:** `10.md`
- **Status:** draft · optional
- **Summary:** ```json ["q", "<event-id> or <event-address>", "<relay-url>", "<pubkey-if-a-regular-event>"] ```
- **Key mechanics:** Abstract, Marked "e" tags (PREFERRED), The "p" tag, Deprecated Positional "e" tags
- **Event kinds / objects:** 1
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec. Document this behavior in our relay-connection guide so ingestion daemons negotiate capabilities and auth correctly.

### NIP-11 — Relay Information Document
- **Source file:** `11.md`
- **Status:** draft · optional · relay
- **Summary:** ```json { "name": <string identifying relay>, "description": <string with detailed information>, "banner": <a link to an image (e.g. in .jpg, or .png format)>, "icon": <a link to an icon (e.g. in .jpg, or .png format>, "pubkey": <administrative contact pubkey>, "self": <relay's own pubkey>, "contact": <administrative alternate contact>, "supported_nips": <a list of NIP numbers supported by the relay>, "software": <string identifying relay software URL>, "version": <string version identifier>, "terms_of_service": <a link to a text file describing the relay's term of service> } ```
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Document this behavior in our relay-connection guide so ingestion daemons negotiate capabilities and auth correctly.

### NIP-12 — NIP-12
- **Source file:** `12.md`
- **Status:** final · mandatory
- **Summary:** (Upstream spec does not include a narrative summary.)
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Primarily client-facing semantics; keep this documented so future features interpret these events/tags consistently.

### NIP-13 — Proof of Work
- **Source file:** `13.md`
- **Status:** draft · optional · relay
- **Summary:** ```json {"content": "It's just me mining my own business", "tags": [["nonce", "1", "21"]]} ```
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Primarily client-facing semantics; keep this documented so future features interpret these events/tags consistently.

### NIP-14 — Subject tag in text events
- **Source file:** `14.md`
- **Status:** draft · optional
- **Summary:** This NIP defines the use of the "subject" tag in text (kind: 1) events. (implemented in more-speech)
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Primarily client-facing semantics; keep this documented so future features interpret these events/tags consistently.

### NIP-15 — Nostr Marketplace (for resilient marketplaces)
- **Source file:** `15.md`
- **Status:** draft · optional
- **Summary:** - `merchant` - seller of products with NOSTR key-pair - `customer` - buyer of products with NOSTR key-pair - `product` - item for sale by the `merchant` - `stall` - list of products controlled by `merchant` (a `merchant` can have multiple stalls) - `marketplace` - clientside software for searching `stalls` and purchasing `products`
- **Key mechanics:** Terms, Nostr Marketplace Clients, `Merchant` publishing/updating products (event), Checkout events
- **Event kinds / objects:** 1021, 1022
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec. Capture payment/commerce metadata verbatim and expose a consistent schema, even if nostr-api does not process money flows.

### NIP-16 — NIP-16
- **Source file:** `16.md`
- **Status:** final · mandatory
- **Summary:** (Upstream spec does not include a narrative summary.)
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Primarily client-facing semantics; keep this documented so future features interpret these events/tags consistently.

### NIP-17 — Private Direct Messages
- **Source file:** `17.md`
- **Status:** draft · optional · relay
- **Summary:** ```jsonc { "id": "<usual hash>", "pubkey": "<sender-pubkey>", "created_at": "<current-time>", "kind": 14, "tags": [ ["p", "<receiver-1-pubkey>", "<relay-url>"], ["p", "<receiver-2-pubkey>", "<relay-url>"], ["e", "<kind-14-id>", "<relay-url>"] // if this is a reply ["subject", "<conversation-title>"], // rest of tags... ], "content": "<message-in-plain-text>", } ```
- **Key mechanics:** Kind Definitions, File Message, Chat Rooms, Encrypting
- **Event kinds / objects:** 7, 13, 14, 15, 1059, 10050
- **Implementation focus (nostr-api):** Store them as replaceable list-style events; keep only the latest per pubkey/kind so stats stay accurate. Document this behavior in our relay-connection guide so ingestion daemons negotiate capabilities and auth correctly.

### NIP-18 — Reposts
- **Source file:** `18.md`
- **Status:** draft · optional
- **Summary:** A repost is a `kind 6` event that is used to signal to followers that a `kind 1` text note is worth reading.
- **Key mechanics:** Quote Reposts, Generic Reposts
- **Event kinds / objects:** 1, 6, 16
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec.

### NIP-19 — bech32-encoded entities
- **Source file:** `19.md`
- **Status:** draft · optional
- **Summary:** - `npub`: public keys - `nsec`: private keys - `note`: note ids
- **Key mechanics:** Bare keys and ids, Shareable identifiers with extra metadata, Examples, Notes
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Primarily client-facing semantics; keep this documented so future features interpret these events/tags consistently.

### NIP-20 — NIP-20
- **Source file:** `20.md`
- **Status:** final · mandatory
- **Summary:** (Upstream spec does not include a narrative summary.)
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Primarily client-facing semantics; keep this documented so future features interpret these events/tags consistently.

### NIP-21 — `nostr:` URI scheme
- **Source file:** `21.md`
- **Status:** draft · optional
- **Summary:** - `nostr:npub1sn0wdenkukak0d9dfczzeacvhkrgz92ak56egt7vdgzn8pv2wfqqhrjdv9` - `nostr:nprofile1qqsrhuxx8l9ex335q7he0f09aej04zpazpl0ne2cgukyawd24mayt8gpp4mhxue69uhhytnc9e3k7mgpz4mhxue69uhkg6nzv9ejuumpv34kytnrdaksjlyr9p` - `nostr:note1fntxtkcy9pjwucqwa9mddn7v03wwwsu9j330jj350nvhpky2tuaspk6nqc` - `nostr:nevent1qqstna2yrezu5wghjvswqqculvvwxsrcvu7uc0f78gan4xqhvz49d9spr3mhxue69uhkummnw3ez6un9d3shjtn4de6x2argwghx6egpr4mhxue69uhkummnw3ez6ur4vgh8wetvd3hhyer9wghxuet5nxnepm`
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** 30023
- **Implementation focus (nostr-api):** Treat these as replaceable/addressable records (dedupe per pubkey + kind + optional d-tag) and surface them via filters in the API.

### NIP-22 — Comment
- **Source file:** `22.md`
- **Status:** draft · optional
- **Summary:** Comments MUST point to the root scope using uppercase tag names (e.g. `K`, `E`, `A` or `I`) and MUST point to the parent item with lowercase ones (e.g. `k`, `e`, `a` or `i`).
- **Key mechanics:** Examples
- **Event kinds / objects:** 1, 1111
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec.

### NIP-23 — Long-form Content
- **Source file:** `23.md`
- **Status:** draft · optional
- **Summary:** - `"title"`, for the article title - `"image"`, for a URL pointing to an image to be shown along with the title - `"summary"`, for the article summary - `"published_at"`, for the timestamp in unix seconds (stringified) of the first time the article was published
- **Key mechanics:** Example Event
- **Event kinds / objects:** 1, 1111, 30023, 30024
- **Implementation focus (nostr-api):** Treat these as replaceable/addressable records (dedupe per pubkey + kind + optional d-tag) and surface them via filters in the API.

### NIP-24 — Extra metadata fields and tags
- **Source file:** `24.md`
- **Status:** draft · optional
- **Summary:** - `display_name`: an alternative, bigger name with richer characters than `name`. `name` should always be set regardless of the presence of `display_name` in the metadata. - `website`: a web URL related in any way to the event author. - `banner`: an URL to a wide (~1024x768) picture to be optionally displayed in the background of a profile screen. - `bot`: a boolean to clarify that the content is entirely or partially the result of automation, such as with chatbots or newsfeeds. - `birthday`: an object representing the author's birth date. The format is { "year": number, "month": number, "day": number }. Each field MAY be omitted.
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** 0, 3
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec. Update client/relay auth flows accordingly; our API should expose hooks to surface auth requirements or errors to operators. Group/channel events imply additional indexing by shared identifiers; expose convenience filters for operators.

### NIP-25 — Reactions
- **Source file:** `25.md`
- **Status:** draft · optional
- **Summary:** A reaction is a `kind 7` event that is used to indicate user reactions to other events. A reaction's `content` field MUST include user-generated-content indicating the value of the reaction (conventionally `+`, `-`, or an emoji).
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** 7, 17
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec.

### NIP-26 — Delegated Event Signing
- **Source file:** `26.md`
- **Status:** draft · optional · relay
- **Upstream note:** **unrecommended**: adds unnecessary burden for little gain
- **Summary:** ```json [ "delegation", <pubkey of the delegator>, <conditions query string>, <delegation token: 64-byte Schnorr signature of the sha256 hash of the delegation string> ] ```
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** 0, 1, 2
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec.

### NIP-27 — Text Note References
- **Source file:** `27.md`
- **Status:** draft · optional
- **Summary:** ```json { "content": "hello nostr:nprofile1qqszclxx9f5haga8sfjjrulaxncvkfekj097t6f3pu65f86rvg49ehqj6f9dh", "created_at": 1679790774, "id": "f39e9b451a73d62abc5016cffdd294b1a904e2f34536a208874fe5e22bbd47cf", "kind": 1, "pubkey": "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798", "sig": "f8c8bab1b90cc3d2ae1ad999e6af8af449ad8bb4edf64807386493163e29162b5852a796a8f474d6b1001cddbaac0de4392838574f5366f03cc94cf5dfb43f4d", "tags": [ [ "p", "2c7cc62a697ea3a7826521f3fd34f0cb273693cbe5e9310f35449f43622a5cdc" ] ] } ```
- **Key mechanics:** Example of a profile mention process, Verbose and probably unnecessary considerations
- **Event kinds / objects:** 1, 30023
- **Implementation focus (nostr-api):** Treat these as replaceable/addressable records (dedupe per pubkey + kind + optional d-tag) and surface them via filters in the API.

### NIP-28 — Public Chat
- **Source file:** `28.md`
- **Status:** draft · optional
- **Summary:** - `40 - channel create` - `41 - channel metadata` - `42 - channel message` - `43 - hide message` - `44 - mute user`
- **Key mechanics:** Kind 40: Create channel, Kind 41: Set channel metadata, Kind 42: Create channel message, Kind 43: Hide message
- **Event kinds / objects:** 40, 41
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec. Group/channel events imply additional indexing by shared identifiers; expose convenience filters for operators.

### NIP-29 — Relay-based Groups
- **Source file:** `29.md`
- **Status:** draft · optional · relay
- **Summary:** ```json { "kind": 9021, "content": "optional reason", "tags": [ ["h", "<group-id>"], ["code", "<optional-invite-code>"] ] } ```
- **Key mechanics:** Relay-generated events, Group identifier, The `h` tag, Timeline references
- **Event kinds / objects:** 9000, 9001, 9009, 9021, 9022, 10009, 39000, 39001, 39002, 39003
- **Implementation focus (nostr-api):** Treat these as replaceable/addressable records (dedupe per pubkey + kind + optional d-tag) and surface them via filters in the API. Document this behavior in our relay-connection guide so ingestion daemons negotiate capabilities and auth correctly. Group/channel events imply additional indexing by shared identifiers; expose convenience filters for operators.

### NIP-30 — Custom Emoji
- **Source file:** `30.md`
- **Status:** draft · optional
- **Summary:** ``` ["emoji", <shortcode>, <image-url>, <emoji-set-address>] ```
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** 0, 1, 7, 30030, 30315
- **Implementation focus (nostr-api):** Treat these as replaceable/addressable records (dedupe per pubkey + kind + optional d-tag) and surface them via filters in the API.

### NIP-31 — Dealing with Unknown Events
- **Source file:** `31.md`
- **Status:** draft · optional
- **Summary:** (Upstream spec does not include a narrative summary.)
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** 1
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec.

### NIP-32 — Labeling
- **Source file:** `32.md`
- **Status:** draft · optional
- **Summary:** - `L` denotes a label namespace - `l` denotes a label
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** 1, 1985
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec.

### NIP-33 — NIP-33
- **Source file:** `33.md`
- **Status:** final · mandatory
- **Summary:** (Upstream spec does not include a narrative summary.)
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Primarily client-facing semantics; keep this documented so future features interpret these events/tags consistently.

### NIP-34 — `git` stuff
- **Source file:** `34.md`
- **Status:** draft · optional
- **Summary:** ```jsonc { "kind": 30617, "content": "", "tags": [ ["d", "<repo-id>"], // usually kebab-case short name ["name", "<human-readable project name>"], ["description", "brief human-readable project description>"], ["web", "<url for browsing>", ...], // a webpage url, if the git server being used provides such a thing ["clone", "<url for git-cloning>", ...], // a url to be given to `git clone` so anyone can clone it ["relays", "<relay-url>", ...], // relays that this repository will monitor for patches and issues ["r", "<earliest-unique-commit-id>", "euc"], ["maintainers", "<other-recognized-maintainer>", ...], ["t","personal-fork"], // optionally indicate author isn't a maintainer ["t", "<arbitrary string>"], // hashtags labelling the repository ] } ```
- **Key mechanics:** Repository announcements, Repository state announcements, Patches and Pull Requests (PRs), Issues
- **Event kinds / objects:** 1617, 1618, 1621
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec. Document this behavior in our relay-connection guide so ingestion daemons negotiate capabilities and auth correctly. Update client/relay auth flows accordingly; our API should expose hooks to surface auth requirements or errors to operators.

### NIP-35 — Torrents
- **Source file:** `35.md`
- **Status:** draft · optional
- **Summary:** - `x`: V1 BitTorrent Info Hash, as seen in the [magnet link](https://www.bittorrent.org/beps/bep_0053.html) `magnet:?xt=urn:btih:HASH` - `file`: A file entry inside the torrent, including the full path ie. `info/example.txt` - `tracker`: (Optional) A tracker to use for this torrent
- **Key mechanics:** Tags, Tag prefixes, Torrent Comments, Implementations
- **Event kinds / objects:** 1, 2003, 2004
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec. Ensure our HTTP ingest/egress surfaces follow this contract when bridging to relays.

### NIP-36 — Sensitive Content
- **Source file:** `36.md`
- **Status:** draft · optional
- **Summary:** The `content-warning` tag enables users to specify if the event's content needs to be approved by readers to be shown. Clients can hide the content until the user acts on it.
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Primarily client-facing semantics; keep this documented so future features interpret these events/tags consistently.

### NIP-37 — Draft Events
- **Source file:** `37.md`
- **Status:** draft · optional
- **Summary:** ```js { "kind": 31234, "tags": [ ["d", "<identifier>"], ["k", "<kind of the draft event>"], // required ["expiration", "now + 90 days"] // recommended ], "content": nip44Encrypt(JSON.stringify(draft_event)), // other fields } ```
- **Key mechanics:** Relay List for Private Content
- **Event kinds / objects:** 10013, 31234
- **Implementation focus (nostr-api):** Treat these as replaceable/addressable records (dedupe per pubkey + kind + optional d-tag) and surface them via filters in the API.

### NIP-38 — User Statuses
- **Source file:** `38.md`
- **Status:** draft · optional
- **Summary:** ```json { "kind": 30315, "content": "Sign up for nostrasia!", "tags": [ ["d", "general"], ["r", "https://nostr.world"] ], } ```
- **Key mechanics:** Abstract, Live Statuses, Client behavior, Use Cases
- **Event kinds / objects:** 30315
- **Implementation focus (nostr-api):** Treat these as replaceable/addressable records (dedupe per pubkey + kind + optional d-tag) and surface them via filters in the API. Ensure our HTTP ingest/egress surfaces follow this contract when bridging to relays.

### NIP-39 — External Identities in Profiles
- **Source file:** `39.md`
- **Status:** draft · optional
- **Summary:** ```jsonc { "kind": 10011, "tags": [ ["i", "github:semisol", "9721ce4ee4fceb91c9711ca2a6c9a5ab"], ["i", "twitter:semisol_public", "1619358434134196225"], ["i", "mastodon:bitcoinhackers.org/@semisol", "109775066355589974"] ["i", "telegram:1087295469", "nostrdirectory/770"] ], // other fields... } ```
- **Key mechanics:** Abstract, Claim types
- **Event kinds / objects:** 10011
- **Implementation focus (nostr-api):** Store them as replaceable list-style events; keep only the latest per pubkey/kind so stats stay accurate.

### NIP-40 — Expiration Timestamp
- **Source file:** `40.md`
- **Status:** draft · optional · relay
- **Summary:** ``` tag: expiration values: - [UNIX timestamp in seconds]: required ```
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Primarily client-facing semantics; keep this documented so future features interpret these events/tags consistently.

### NIP-42 — Authentication of clients to relays
- **Source file:** `42.md`
- **Status:** draft · optional · relay
- **Summary:** - A relay may request payment or other forms of whitelisting to publish events -- this can naïvely be achieved by limiting publication to events signed by the whitelisted key, but with this NIP they may choose to accept any events as long as they are published from an authenticated user; - A relay may limit access to `kind: 4` DMs to only the parties involved in the chat exchange, and for that it may require authentication before clients can query for that kind. - A relay may limit subscriptions of any kind to paying users or users whitelisted through any other means, and require authentication.
- **Key mechanics:** Motivation, Definitions, Protocol flow, Signed Event Verification
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Document this behavior in our relay-connection guide so ingestion daemons negotiate capabilities and auth correctly. Update client/relay auth flows accordingly; our API should expose hooks to surface auth requirements or errors to operators. Capture payment/commerce metadata verbatim and expose a consistent schema, even if nostr-api does not process money flows. Group/channel events imply additional indexing by shared identifiers; expose convenience filters for operators.

### NIP-43 — Relay Access Metadata and Requests
- **Source file:** `43.md`
- **Status:** draft · optional · relay
- **Summary:** - A [NIP 70](./70.md) `-` tag - A `member` tag containing a hex pubkey should be included for each member
- **Key mechanics:** Membership Lists, Add User, Remove User, Join Request
- **Event kinds / objects:** 8000, 8001, 10010, 13534, 28934, 28935, 28936
- **Implementation focus (nostr-api):** Store them as replaceable list-style events; keep only the latest per pubkey/kind so stats stay accurate. Document this behavior in our relay-connection guide so ingestion daemons negotiate capabilities and auth correctly.

### NIP-44 — Encrypted Payloads (Versioned)
- **Source file:** `44.md`
- **Status:** optional
- **Summary:** The NIP introduces a new data format for keypair-based encryption. This NIP is versioned to allow multiple algorithm choices to exist simultaneously. This format may be used for many things, but MUST be used in the context of a signed event as described in NIP-01.
- **Key mechanics:** Versions, Limitations, Version 2
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Do not mutate ciphertext; just store bytes and index metadata so authorized clients can decrypt off-box.

### NIP-45 — Counting results
- **Source file:** `45.md`
- **Status:** draft · optional · relay
- **Summary:** ``` ["COUNT", <query_id>, <filters JSON>...] ```
- **Key mechanics:** Motivation, Filters and return values, HyperLogLog, Examples
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Primarily client-facing semantics; keep this documented so future features interpret these events/tags consistently.

### NIP-46 — Nostr Remote Signing
- **Source file:** `46.md`
- **Summary:** - **user**: A person that is trying to use Nostr. - **client**: A user-facing application that _user_ is looking at and clicking buttons in. This application will send requests to _remote-signer_. - **remote-signer**: A daemon or server running somewhere that will answer requests from _client_, also known as "bunker". - **client-keypair/pubkey**: The keys generated by _client_. Used to encrypt content and communicate with _remote-signer_. - **remote-signer-keypair/pubkey**: The keys used by _remote-signer_ to encrypt content and communicate with _client_. This keypair MAY be same as _user-keypair_, but not necessarily. - **user-keypair/pubkey**: The actual keys representing _user_ (that will be used to sign events in response to `sign_event` requests, for example). The _remote-signer_ generally has control over these keys.
- **Key mechanics:** Changes, Rationale, Terminology, Overview
- **Event kinds / objects:** 4, 24133
- **Implementation focus (nostr-api):** Store them as replaceable list-style events; keep only the latest per pubkey/kind so stats stay accurate.

### NIP-47 — Nostr Wallet Connect
- **Source file:** `47.md`
- **Status:** draft · optional
- **Summary:** * **client**: Nostr app on any platform that wants to interact with a lightning wallet. * **user**: The person using the **client**, and wants to connect their wallet to their **client**. * **wallet service**: Nostr app that typically runs on an always-on computer (eg. in the cloud or on a Raspberry Pi).  This app has access to the APIs of the wallets it serves.
- **Key mechanics:** Rationale, Terms, Theory of Operation, Events
- **Event kinds / objects:** 9734, 23194, 23195, 23196, 23197
- **Implementation focus (nostr-api):** Store them as replaceable list-style events; keep only the latest per pubkey/kind so stats stay accurate. Capture payment/commerce metadata verbatim and expose a consistent schema, even if nostr-api does not process money flows.

### NIP-48 — Proxy Tags
- **Source file:** `48.md`
- **Status:** draft · optional
- **Summary:** ``` ["proxy", <id>, <protocol>] ```
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Primarily client-facing semantics; keep this documented so future features interpret these events/tags consistently.

### NIP-49 — Private Key Encryption
- **Source file:** `49.md`
- **Status:** draft · optional
- **Summary:** | LOG_N | MEMORY REQUIRED | APPROX TIME ON FAST COMPUTER | |-------|-----------------|----------------------------- | | 16    | 64 MiB          | 100 ms                       | | 18    | 256 MiB         |                              | | 20    | 1 GiB           | 2 seconds                    | | 21    | 2 GiB           |                              | | 22    | 4 GiB           |                              |
- **Key mechanics:** Password Unicode Normalization, Encryption, Decryption
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Primarily client-facing semantics; keep this documented so future features interpret these events/tags consistently.

### NIP-50 — Search Capability
- **Source file:** `50.md`
- **Status:** draft · optional · relay
- **Summary:** Many Nostr use cases require some form of general search feature, in addition to structured queries by tags or ids. Specifics of the search algorithms will differ between event kinds, this NIP only describes a general extensible framework for performing such queries.
- **Key mechanics:** Abstract, `search` filter field, Extensions
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Primarily client-facing semantics; keep this documented so future features interpret these events/tags consistently.

### NIP-51 — Lists
- **Source file:** `51.md`
- **Status:** draft · optional
- **Summary:** | name              | kind  | description                                                 | expected tag items                                                                                  | | ---               | ---   | ---                                                         | ---                                                                                                 | | Follow list       |     3 | microblogging basic follow list, see [NIP-02](02.md)        | `"p"` (pubkeys -- with optional relay hint and petname)                                             | | Mute list         | 10000 | things the user doesn't want to see in their feeds          | `"p"` (pubkeys), `"t"` (hashtags), `"word"` (lowercase string), `"e"` (threads)                     | | Pinned notes      | 10001 | events the user intends to showcase in their profile page   | `"e"` (kind:1 notes)                                                                                | | Read/write relays | 10002 | where a user publishes to and where they expect mentions    | see [NIP-65](65.md)                                                                                 | | Bookmarks         | 10003 | uncategorized, "global" list of things a user wants to save | `"e"` (kind:1 notes), `"a"` (kind:30023 articles)                   | | Communities       | 10004 | [NIP-72](72.md) communities the user belongs to             | `"a"` (kind:34550 community definitions)                                                            | | Public chats      | 10005 | [NIP-28](28.md) chat channels the user is in                | `"e"` (kind:40 channel definitions)                                                                 | | Blocked relays    | 10006 | relays clients should never connect to                      | `"relay"` (relay URLs)                                                                              | | Search relays     | 10007 | relays clients should use when performing search queries    | `"relay"` (relay URLs)                                                                              | | Simple groups     | 10009 | [NIP-29](29.md) groups the user is in                       | `"group"` ([NIP-29](29.md) group id + relay URL + optional group name), `"r"` for each relay in use | | Relay feeds       | 10012 | user favorite browsable relays (and relay sets)             | `"relay"` (relay URLs) and `"a"` (kind:30002 relay set)                                             | | Interests         | 10015 | topics a user may be interested in and pointers             | `"t"` (hashtags) and `"a"` (kind:30015 interest set)                                                | | Media follows     | 10020 | multimedia (photos, short video) follow list                | `"p"` (pubkeys -- with optional relay hint and petname)                                             | | Emojis            | 10030 | user preferred emojis and pointers to emoji sets            | `"emoji"` (see [NIP-30](30.md)) and `"a"` (kind:30030 emoji set)                                    | | DM relays         | 10050 | Where to receive [NIP-17](17.md) direct messages            | `"relay"` (see [NIP-17](17.md))                                                                     | | Good wiki authors | 10101 | [NIP-54](54.md) user recommended wiki authors               | `"p"` (pubkeys)                                                                                     | | Good wiki relays  | 10102 | [NIP-54](54.md) relays deemed to only host useful articles  | `"relay"` (relay URLs)                                                                              |
- **Key mechanics:** Types of lists, Examples, Encryption process pseudocode
- **Event kinds / objects:** 1, 20, 21, 40, 1063, 10000, 10001, 10003, 10004, 30002, 30015, 30023, 30030, 34550
- **Implementation focus (nostr-api):** Treat these as replaceable/addressable records (dedupe per pubkey + kind + optional d-tag) and surface them via filters in the API. Document this behavior in our relay-connection guide so ingestion daemons negotiate capabilities and auth correctly. Update client/relay auth flows accordingly; our API should expose hooks to surface auth requirements or errors to operators. Group/channel events imply additional indexing by shared identifiers; expose convenience filters for operators.

### NIP-52 — Calendar Events
- **Source file:** `52.md`
- **Status:** draft · optional
- **Summary:** * `d` (required) a short unique string identifier. Generated by the client creating the calendar event. * `title` (required) title of the calendar event * `summary` (optional) brief description of the calendar event * `image` (optional) url of an image to use for the event * `location` (optional, repeated) location of the calendar event. e.g. address, GPS coordinates, meeting room name, link to video call * `g` (optional) [geohash](https://en.wikipedia.org/wiki/Geohash) to associate calendar event with a searchable physical location * `p` (optional, repeated) 32-bytes hex pubkey of a participant, optional recommended relay URL, and participant's role in the meeting * `t` (optional, repeated) hashtag to categorize calendar event * `r` (optional, repeated) references / links to web pages, documents, video calls, recorded videos, etc. * `a` (repeated) reference tag to kind `31924` calendar event requesting to be included in Calendar
- **Key mechanics:** Calendar Events, Collaborative Calendar Event Requests, Calendar, Calendar Event RSVP
- **Event kinds / objects:** 31922, 31923, 31924, 31925
- **Implementation focus (nostr-api):** Treat these as replaceable/addressable records (dedupe per pubkey + kind + optional d-tag) and surface them via filters in the API. Document this behavior in our relay-connection guide so ingestion daemons negotiate capabilities and auth correctly. Ensure our HTTP ingest/egress surfaces follow this contract when bridging to relays.

### NIP-53 — Live Activities
- **Source file:** `53.md`
- **Status:** draft · optional
- **Summary:** ```jsonc { "kind": 30311, "tags": [ ["d", "<unique identifier>"], ["title", "<name of the event>"], ["summary", "<description>"], ["image", "<preview image url>"], ["t", "hashtag"] ["streaming", "<url>"], ["recording", "<url>"], // used to place the edited video once the activity is over ["starts", "<unix timestamp in seconds>"], ["ends", "<unix timestamp in seconds>"], ["status", "<planned, live, ended>"], ["current_participants", "<number>"], ["total_participants", "<number>"], ["p", "91cf9..4e5ca", "wss://provider1.com/", "Host", "<proof>"], ["p", "14aeb..8dad4", "wss://provider2.com/nostr", "Speaker"], ["p", "612ae..e610f", "ws://provider3.com/ws", "Participant"], ["relays", "wss://one.com", "wss://two.com", /*...*/], ["pinned", "<event id of pinned live chat message>"], ], "content": "", // other fields... } ```
- **Key mechanics:** Live Streaming, Meeting Spaces
- **Event kinds / objects:** 1311, 10312, 30311, 30312, 30313
- **Implementation focus (nostr-api):** Treat these as replaceable/addressable records (dedupe per pubkey + kind + optional d-tag) and surface them via filters in the API. Document this behavior in our relay-connection guide so ingestion daemons negotiate capabilities and auth correctly. Group/channel events imply additional indexing by shared identifiers; expose convenience filters for operators.

### NIP-54 — Wiki
- **Source file:** `54.md`
- **Status:** draft · optional
- **Summary:** ```json { "content": "A wiki is a hypertext publication collaboratively edited and managed by its own audience.", "tags": [ ["d", "wiki"], ["title", "Wiki"] ] } ```
- **Key mechanics:** Articles, `d` tag normalization rules, Content, Optional extra tags
- **Event kinds / objects:** 818, 10102, 30818, 30819
- **Implementation focus (nostr-api):** Treat these as replaceable/addressable records (dedupe per pubkey + kind + optional d-tag) and surface them via filters in the API.

### NIP-55 — Android Signer Application
- **Source file:** `55.md`
- **Status:** draft · optional
- **Summary:** ```xml <queries> <intent> <action android:name="android.intent.action.VIEW" /> <category android:name="android.intent.category.BROWSABLE" /> <data android:scheme="nostrsigner" /> </intent> </queries> ```
- **Key mechanics:** Usage for Android applications, Usage for Web Applications
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Primarily client-facing semantics; keep this documented so future features interpret these events/tags consistently.

### NIP-56 — Reporting
- **Source file:** `56.md`
- **Status:** optional
- **Summary:** A report is a `kind 1984` event that signals to users and relays that some referenced content is objectionable. The definition of objectionable is obviously subjective and all agents on the network (users, apps, relays, etc.) may consume and take action on them as they see fit.
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** 1984
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec. Document this behavior in our relay-connection guide so ingestion daemons negotiate capabilities and auth correctly.

### NIP-57 — Lightning Zaps
- **Source file:** `57.md`
- **Status:** draft · optional
- **Summary:** 1. Client calculates a recipient's lnurl pay request url from the `zap` tag on the event being zapped (see Appendix G), or by decoding their lud16 field on their profile according to the [lnurl specifications](https://github.com/lnurl/luds). The client MUST send a GET request to this url and parse the response. If `allowsNostr` exists and it is `true`, and if `nostrPubkey` exists and is a valid BIP 340 public key in hex, the client should associate this information with the user, along with the response's `callback`, `minSendable`, and `maxSendable` values. 2. Clients may choose to display a lightning zap button on each post or on a user's profile. If the user's lnurl pay request endpoint supports nostr, the client SHOULD use this NIP to request a `zap receipt` rather than a normal lnurl invoice. 3. When a user (the "sender") indicates they want to send a zap to another user (the "recipient"), the client should create a `zap request` event as described in Appendix A of this NIP and sign it. 4. Instead of publishing the `zap request`, the `9734` event should instead be sent to the `callback` url received from the lnurl pay endpoint for the recipient using a GET request. See Appendix B for details and an example. 5. The recipient's lnurl server will receive this `zap request` and validate it. See Appendix C for details on how to properly configure an lnurl server to support zaps, and Appendix D for details on how to validate the `nostr` query parameter. 6. If the `zap request` is valid, the server should fetch a description hash invoice where the description is this `zap request` note and this note only. No additional lnurl metadata is included in the description. This will be returned in the response according to [LUD06](https://github.com/lnurl/luds/blob/luds/06.md). 7. On receiving the invoice, the client MAY pay it or pass it to an app that can pay the invoice. 8. Once the invoice is paid, the recipient's lnurl server MUST generate a `zap receipt` as described in Appendix E, and publish it to the `relays` specified in the `zap request`. 9. Clients MAY fetch `zap receipt`s on posts and profiles, but MUST authorize their validity as described in Appendix F. If the `zap request` note contains a non-empty `content`, it may display a zap comment. Generally clients should show users the `zap request` note, and use the `zap receipt` to show "zap authorized by ..." but this is optional.
- **Key mechanics:** Protocol flow, Reference and examples, Future Work
- **Event kinds / objects:** 9734, 9735
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec. Document this behavior in our relay-connection guide so ingestion daemons negotiate capabilities and auth correctly. Update client/relay auth flows accordingly; our API should expose hooks to surface auth requirements or errors to operators. Ensure our HTTP ingest/egress surfaces follow this contract when bridging to relays. Capture payment/commerce metadata verbatim and expose a consistent schema, even if nostr-api does not process money flows.

### NIP-58 — Badges
- **Source file:** `58.md`
- **Status:** draft · optional
- **Summary:** Three special events are used to define, award and display badges in user profiles:
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** 8, 30008, 30009
- **Implementation focus (nostr-api):** Treat these as replaceable/addressable records (dedupe per pubkey + kind + optional d-tag) and surface them via filters in the API.

### NIP-59 — Gift Wrap
- **Source file:** `59.md`
- **Status:** optional · relay
- **Summary:** This NIP defines a protocol for encapsulating any nostr event. This makes it possible to obscure most metadata for a given event, perform collaborative signing, and more.
- **Key mechanics:** Overview, Protocol Description, Encrypting Payloads, Other Considerations
- **Event kinds / objects:** 1, 13, 1059
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec.

### NIP-60 — Cashu Wallet
- **Source file:** `60.md`
- **Status:** draft · optional
- **Summary:** The purpose of this NIP is: * ease-of-use: new users immediately are able to receive funds without creating accounts with other services. * interoperability: users' wallets follows them across applications.
- **Key mechanics:** High-level flow, Flow, Appendix 1: Validating proofs
- **Event kinds / objects:** 5, 7375, 7376, 10019, 17375
- **Implementation focus (nostr-api):** Store them as replaceable list-style events; keep only the latest per pubkey/kind so stats stay accurate. Capture payment/commerce metadata verbatim and expose a consistent schema, even if nostr-api does not process money flows.

### NIP-61 — Nutzaps
- **Source file:** `61.md`
- **Status:** draft · optional
- **Summary:** 1. Alice fetches event `kind:10019` from Bob to see the mints Bob trusts. 2. She mints a token at that mint (or swaps some tokens she already had in that mint) P2PK-locked to the pubkey Bob has listed in his `kind:10019`. 3. She publishes a `kind:9321` event to the relays Bob indicated with the proofs she minted.
- **Key mechanics:** High-level flow, Nutzap informational event, Sending a nutzap, Receiving nutzaps
- **Event kinds / objects:** 7376, 9321, 10019
- **Implementation focus (nostr-api):** Store them as replaceable list-style events; keep only the latest per pubkey/kind so stats stay accurate. Document this behavior in our relay-connection guide so ingestion daemons negotiate capabilities and auth correctly.

### NIP-62 — Request to Vanish
- **Source file:** `62.md`
- **Status:** draft · optional · relay
- **Summary:** ```jsonc { "kind": 62, "pubkey": <32-byte hex-encoded public key of the event creator>, "tags": [ ["relay", "<relay url>"] ], "content": "<reason or note>", //...other fields } ```
- **Key mechanics:** Request to Vanish from Relay, Global Request to Vanish
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Document this behavior in our relay-connection guide so ingestion daemons negotiate capabilities and auth correctly.

### NIP-64 — Chess (PGN)
- **Source file:** `64.md`
- **Status:** draft · optional
- **Summary:** ```jsonc { "kind": 64, "content": "1. e4 *", // other fields... } ```
- **Key mechanics:** Note, Client Behavior, Relay Behavior, Examples
- **Event kinds / objects:** 64
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec.

### NIP-65 — Relay List Metadata
- **Source file:** `65.md`
- **Status:** draft · optional
- **Summary:** ```jsonc { "kind": 10002, "tags": [ ["r", "wss://alicerelay.example.com"], ["r", "wss://brando-relay.com"], ["r", "wss://expensive-relay.example2.com", "write"], ["r", "wss://nostr-relay.example.com", "read"] ], "content": "", // other fields... } ```
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** 10002
- **Implementation focus (nostr-api):** Store them as replaceable list-style events; keep only the latest per pubkey/kind so stats stay accurate. Document this behavior in our relay-connection guide so ingestion daemons negotiate capabilities and auth correctly.

### NIP-66 — Relay Discovery and Liveness Monitoring
- **Source file:** `66.md`
- **Status:** draft · optional · relay
- **Summary:** - `rtt-open` - The relay's open round-trip time in milliseconds. - `rtt-read` - The relay's read round-trip time in milliseconds. - `rtt-write` - The relay's write round-trip time in milliseconds. - `n` - The relay's network type. SHOULD be one of `clearnet`, `tor`, `i2p`, `loki` - `T` - The relay type. Enumerated [relay type](https://github.com/nostr-protocol/nips/issues/1282) formatted as `PascalCase`, e.g. `PrivateInbox` - `N` - NIPs supported by the relay - `R` - Keys corresponding to requirements per [NIP 11](https://github.com/nostr-protocol/nips/blob/master/11.md)'s `limitations` array, including `auth`, `writes`, `pow`, and `payment`. False values should be specified using a `!` prefix, for example `!auth`. - `t` - A topic associated with this relay - `k` - Accepted and unaccepted kinds (false values prepended by `!`) - `g` - A [NIP-52](https://github.com/nostr-protocol/nips/blob/master/52.md) geohash
- **Key mechanics:** Relay Discovery Events, Relay Monitor Announcements
- **Event kinds / objects:** 0, 10002
- **Implementation focus (nostr-api):** Store them as replaceable list-style events; keep only the latest per pubkey/kind so stats stay accurate. Document this behavior in our relay-connection guide so ingestion daemons negotiate capabilities and auth correctly. Update client/relay auth flows accordingly; our API should expose hooks to surface auth requirements or errors to operators. Ensure our HTTP ingest/egress surfaces follow this contract when bridging to relays. Capture payment/commerce metadata verbatim and expose a consistent schema, even if nostr-api does not process money flows.

### NIP-68 — Picture-first feeds
- **Source file:** `68.md`
- **Status:** draft · optional
- **Summary:** ```jsonc { "id": <32-bytes lowercase hex-encoded SHA-256 of the the serialized event data>, "pubkey": <32-bytes lowercase hex-encoded public key of the event creator>, "created_at": <Unix timestamp in seconds>, "kind": 20, "content": "<description of post>", "tags": [ ["title", "<short title of post>"],
- **Key mechanics:** Picture Events
- **Event kinds / objects:** 20, 22
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec.

### NIP-69 — Peer-to-peer Order events
- **Source file:** `69.md`
- **Status:** draft · optional
- **Summary:** ```json { "id": "84fad0d29cb3529d789faeff2033e88fe157a48e071c6a5d1619928289420e31", "pubkey": "dbe0b1be7aafd3cfba92d7463edbd4e33b2969f61bd554d37ac56f032e13355a", "created_at": 1702548701, "kind": 38383, "tags": [ ["d", "ede61c96-4c13-4519-bf3a-dcf7f1e9d842"], ["k", "sell"], ["f", "VES"], ["s", "pending"], ["amt", "0"], ["fa", "100"], ["pm", "face to face", "bank transfer"], ["premium", "1"], [ "rating", "{\"total_reviews\":1,\"total_rating\":3.0,\"last_rating\":3,\"max_rate\":5,\"min_rate\":1}" ], ["source", "https://t.me/p2plightning/xxxxxxx"], ["network", "mainnet"], ["layer", "lightning"], ["name", "Nakamoto"], ["g", "<geohash>"], ["bond", "0"], ["expires_at", "1719391096"], ["expiration", "1719995896"], ["y", "lnp2pbot"], ["z", "order"] ], "content": "", "sig": "7e8fe1eb644f33ff51d8805c02a0e1a6d034e6234eac50ef7a7e0dac68a0414f7910366204fa8217086f90eddaa37ded71e61f736d1838e37c0b73f6a16c4af2" } ```
- **Key mechanics:** Abstract, The event, Tags, Implementations
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Ensure our HTTP ingest/egress surfaces follow this contract when bridging to relays. Capture payment/commerce metadata verbatim and expose a consistent schema, even if nostr-api does not process money flows.

### NIP-70 — Protected Events
- **Source file:** `70.md`
- **Status:** draft · optional · relay
- **Summary:** ```jsonc /* client: */ ["EVENT",{"id":"cb8feca582979d91fe90455867b34dbf4d65e4b86e86b3c68c368ca9f9eef6f2","pubkey":"79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798","created_at":1707409439,"kind":1,"tags":[["-"]],"content":"hello members of the secret group","sig":"fa163f5cfb75d77d9b6269011872ee22b34fb48d23251e9879bb1e4ccbdd8aaaf4b6dc5f5084a65ef42c52fbcde8f3178bac3ba207de827ec513a6aa39fa684c"}] /* relay: */ ["AUTH", "<challenge>"] ["OK", "cb8feca582979d91fe90455867b34dbf4d65e4b86e86b3c68c368ca9f9eef6f2", false, "auth-required: this event may only be published by its author"] /* client: */ ["AUTH", {}] ["EVENT",{"id":"cb8feca582979d91fe90455867b34dbf4d65e4b86e86b3c68c368ca9f9eef6f2","pubkey":"79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798","created_at":1707409439,"kind":1,"tags":[["-"]],"content":"hello members of the secret group","sig":"fa163f5cfb75d77d9b6269011872ee22b34fb48d23251e9879bb1e4ccbdd8aaaf4b6dc5f5084a65ef42c52fbcde8f3178bac3ba207de827ec513a6aa39fa684c"}] ["OK", "cb8feca582979d91fe90455867b34dbf4d65e4b86e86b3c68c368ca9f9eef6f2", true, ""] ```
- **Key mechanics:** The tag, Example flow, Why
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Document this behavior in our relay-connection guide so ingestion daemons negotiate capabilities and auth correctly. Update client/relay auth flows accordingly; our API should expose hooks to surface auth requirements or errors to operators. Group/channel events imply additional indexing by shared identifiers; expose convenience filters for operators.

### NIP-71 — Video Events
- **Source file:** `71.md`
- **Status:** draft · optional
- **Summary:** - Kind `34235` for _addressable normal videos_ - Kind `34236` for _addressable short videos_
- **Key mechanics:** Video Events, Addressable Video Events, Addressable Event Example, Referencing Addressable Events
- **Event kinds / objects:** 1, 21, 6000
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec.

### NIP-72 — Moderated Communities
- **Source file:** `72.md`
- **Status:** draft · optional
- **Summary:** ```jsonc { "created_at": <Unix timestamp in seconds>, "kind": 34550, "tags": [ ["d", "<community-d-identifier>"], ["name", "<Community name>"], ["description", "<Community description>"], ["image", "<Community image url>", "<Width>x<Height>"],
- **Key mechanics:** Community Definition, Posting to a community, Moderation, Cross-posting
- **Event kinds / objects:** 0, 1, 6, 16, 1111, 4550, 34550
- **Implementation focus (nostr-api):** Treat these as replaceable/addressable records (dedupe per pubkey + kind + optional d-tag) and surface them via filters in the API. Group/channel events imply additional indexing by shared identifiers; expose convenience filters for operators.

### NIP-73 — External Content IDs
- **Source file:** `73.md`
- **Status:** draft · optional
- **Summary:** | Type                   | `i` tag                                                    | `k` tag                  | | ---                    | ---                                                        | ---                      | | URLs                   | "`<URL, normalized, no fragment>`"                         | "web"                    | | Books                  | "isbn:`<id, without hyphens>`"                             | "isbn"                   | | Geohashes              | "geo:`<geohash, lowercase>`"                               | "geo"                    | | Countries              | "iso3166:`<code, uppercase>`"                              | "iso3166"                | | Movies                 | "isan:`<id, without version part>`"                        | "isan"                   | | Papers                 | "doi:`<id, lowercase>`"                                    | "doi"                    | | Hashtags               | "#`<topic, lowercase>`"                                    | "#"                      | | Podcast Feeds          | "podcast:guid:`<guid>`"                                    | "podcast:guid"           | | Podcast Episodes       | "podcast:item:guid:`<guid>`"                               | "podcast:item:guid"      | | Podcast Publishers     | "podcast:publisher:guid:`<guid>`"                          | "podcast:publisher:guid" | | Blockchain Transaction | "`<blockchain>`:[`<chainId>`:]tx:`<txid, hex, lowercase>`" | "`<blockchain>`:tx"      | | Blockchain Address     | "`<blockchain>`:[`<chainId>`:]address:`<address>`"         | "`<blockchain>`:address" |
- **Key mechanics:** Supported IDs, Examples
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Primarily client-facing semantics; keep this documented so future features interpret these events/tags consistently.

### NIP-75 — Zap Goals
- **Source file:** `75.md`
- **Status:** draft · optional
- **Summary:** - `amount` - target amount in milisats. - `relays` - a list of relays the zaps to this goal will be sent to and tallied from.
- **Key mechanics:** Nostr Event, Client behavior, Use cases
- **Event kinds / objects:** 9041
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec. Document this behavior in our relay-connection guide so ingestion daemons negotiate capabilities and auth correctly. Capture payment/commerce metadata verbatim and expose a consistent schema, even if nostr-api does not process money flows.

### NIP-77 — Negentropy Syncing
- **Source file:** `77.md`
- **Status:** draft · optional · relay
- **Summary:** * (1) Client (initiator) chooses a filter, and retrieves the set of events that it has locally that match this filter (or uses a cache), and constructs an initial message. * (2) Client sends a `NEG-OPEN` message to the relay, which includes the filter and the initial message. * (3) Relay selects the set of events that it has locally that match the filter (or uses a cache). * (4) Relay constructs a response and returns it to the client in a `NEG-MSG` message. * (5) Client parses the message to learn about IDs it has (and relay needs) and IDs it needs (and relay has). * If client wishes to continue, then it constructs a new message and sends it to the relay in a `NEG-MSG` message. Goto step 4. * If client wishes to stop, then it sends a `NEG-CLOSE` message or disconnects the websocket.
- **Key mechanics:** High-Level Protocol Description, Nostr Messages, Appendix: Negentropy Protocol V1
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Document this behavior in our relay-connection guide so ingestion daemons negotiate capabilities and auth correctly.

### NIP-78 — Application-specific data
- **Source file:** `78.md`
- **Status:** draft · optional
- **Summary:** - User personal settings on Nostr clients (and other apps unrelated to Nostr) - A way for client developers to propagate dynamic parameters to users without these having to update - Personal private data generated by apps that have nothing to do with Nostr, but allow users to use Nostr relays as their personal database
- **Key mechanics:** Nostr event, Some use cases
- **Event kinds / objects:** 30078
- **Implementation focus (nostr-api):** Treat these as replaceable/addressable records (dedupe per pubkey + kind + optional d-tag) and surface them via filters in the API. Document this behavior in our relay-connection guide so ingestion daemons negotiate capabilities and auth correctly.

### NIP-7D — Threads
- **Source file:** `7D.md`
- **Status:** draft · optional
- **Summary:** ```json { "kind": 11, "content": "Good morning", "tags": [ ["title", "GM"] ] } ```
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** 11, 1111
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec.

### NIP-84 — Highlights
- **Source file:** `84.md`
- **Status:** draft · optional
- **Summary:** Events SHOULD tag the source of the highlight, whether nostr-native or not. `a` or `e` tags should be used for nostr events and `r` tags for URLs.
- **Key mechanics:** Format, Quote Highlights
- **Event kinds / objects:** 1, 9802
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec.

### NIP-85 — Trusted Assertions
- **Source file:** `85.md`
- **Status:** draft · optional
- **Summary:** | Subject            | Event Kind     | `d` tag value     | | ------------------ | -------------- | ----------------- | | User               | 30382          | `<pubkey>`        | | Event              | 30383          | `<event_id>`      | | Addressable Event  | 30384          | `<event_address>` | | NIP-73 Identifier  | 30385          | `<i-tag>`         |
- **Key mechanics:** Assertion Events, Kind 30382: Users as Subject:, Kind 30383: Events as Subject, Kind 30384: Addressables as Subject
- **Event kinds / objects:** 0, 10040
- **Implementation focus (nostr-api):** Store them as replaceable list-style events; keep only the latest per pubkey/kind so stats stay accurate.

### NIP-86 — Relay Management API
- **Source file:** `86.md`
- **Status:** draft · optional
- **Summary:** ```json { "method": "<method-name>", "params": ["<array>", "<of>", "<parameters>"] } ```
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Document this behavior in our relay-connection guide so ingestion daemons negotiate capabilities and auth correctly.

### NIP-87 — Ecash Mint Discoverability
- **Source file:** `87.md`
- **Status:** draft · optional
- **Summary:** Nostr's discoverability and transparent event interaction is one of its most interesting/novel mechanics. This NIP provides a simple way for users to discover ecash mints recommended by other users and to interact with them.
- **Key mechanics:** Rationale, Events, Ecash Mint Information, Example
- **Event kinds / objects:** 0, 38000, 38172, 38173
- **Implementation focus (nostr-api):** Treat these as replaceable/addressable records (dedupe per pubkey + kind + optional d-tag) and surface them via filters in the API.

### NIP-88 — Polls
- **Source file:** `88.md`
- **Status:** draft · optional
- **Summary:** - **option**: The option tags contain an OptionId(any alphanumeric) field, followed by an option label field. - **relay**: One or multiple tags that the poll is expecting respondents to respond on. - **polltype**: can be "singlechoice" or "multiplechoice". Polls that do not have a polltype should be considered a "singlechoice" poll. - **endsAt**: signifying at which unix timestamp the poll is meant to end.
- **Key mechanics:** Events
- **Event kinds / objects:** 5, 1018, 1068, 30000
- **Implementation focus (nostr-api):** Treat these as replaceable/addressable records (dedupe per pubkey + kind + optional d-tag) and surface them via filters in the API. Document this behavior in our relay-connection guide so ingestion daemons negotiate capabilities and auth correctly.

### NIP-89 — Recommended Application Handlers
- **Source file:** `89.md`
- **Status:** draft · optional
- **Summary:** Nostr's discoverability and transparent event interaction is one of its most interesting/novel mechanics. This NIP provides a simple way for clients to discover applications that handle events of a specific kind to ensure smooth cross-client and cross-kind interactions.
- **Key mechanics:** Rationale, Events, Client tag, User flow
- **Event kinds / objects:** 0, 1, 31337, 31989, 31990
- **Implementation focus (nostr-api):** Treat these as replaceable/addressable records (dedupe per pubkey + kind + optional d-tag) and surface them via filters in the API.

### NIP-90 — Data Vending Machines
- **Source file:** `90.md`
- **Status:** draft · optional
- **Summary:** | Kind      | Description       | | ----      | -----------       | | 5000-5999 | Job request kinds | | 6000-6999 | Job result        | | 7000      | Job feedback      |
- **Key mechanics:** Kinds, Rationale, Job request (`kind:5000-5999`), Job result (`kind:6000-6999`)
- **Event kinds / objects:** 5, 5000, 5001, 6000, 6001, 7000
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec.

### NIP-92 — Media Attachments
- **Source file:** `92.md`
- **Summary:** The `imeta` tag is variadic, and each entry is a space-delimited key/value pair. Each `imeta` tag MUST have a `url`, and at least one other field. `imeta` MAY include any field specified by [NIP 94](./94.md). There SHOULD be only one `imeta` tag per URL.
- **Key mechanics:** Example, Recommended client behavior
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Primarily client-facing semantics; keep this documented so future features interpret these events/tags consistently.

### NIP-94 — File Metadata
- **Source file:** `94.md`
- **Status:** draft · optional
- **Summary:** * `url` the url to download the file * `m` a string indicating the data type of the file. The [MIME types](https://developer.mozilla.org/en-US/docs/Web/HTTP/Basics_of_HTTP/MIME_types/Common_types) format must be used, and they should be lowercase. * `x` containing the SHA-256 hexencoded string of the file. * `ox` containing the SHA-256 hexencoded string of the original file, before any transformations done by the upload server * `size` (optional) size of file in bytes * `dim` (optional) size of file in pixels in the form `<width>x<height>` * `magnet` (optional) URI to magnet file * `i` (optional) torrent infohash * `blurhash`(optional) the [blurhash](https://github.com/woltapp/blurhash) to show while the file is being loaded by the client * `thumb` (optional) url of thumbnail with same aspect ratio * `image` (optional) url of preview image with same dimensions * `summary` (optional) text excerpt * `alt` (optional) description for accessibility * `fallback` (optional) zero or more fallback file sources in case `url` fails * `service` (optional) service type which is serving the file (eg. [NIP-96](96.md))
- **Key mechanics:** Event format, Suggested use cases
- **Event kinds / objects:** 1, 30023
- **Implementation focus (nostr-api):** Treat these as replaceable/addressable records (dedupe per pubkey + kind + optional d-tag) and surface them via filters in the API. Ensure our HTTP ingest/egress surfaces follow this contract when bridging to relays.

### NIP-96 — HTTP File Storage Integration
- **Source file:** `96.md`
- **Status:** draft · optional
- **Upstream note:** **unrecommended**: replaced by blossom APIs
- **Summary:** This NIP defines a REST API for HTTP file storage servers intended to be used in conjunction with the nostr network. The API will enable nostr users to upload files and later reference them by url on nostr notes.
- **Key mechanics:** Introduction, Server Adaptation, Auth, Upload
- **Event kinds / objects:** 10096
- **Implementation focus (nostr-api):** Store them as replaceable list-style events; keep only the latest per pubkey/kind so stats stay accurate. Ensure our HTTP ingest/egress surfaces follow this contract when bridging to relays.

### NIP-98 — HTTP Auth
- **Source file:** `98.md`
- **Status:** draft · optional
- **Summary:** * `u` - absolute URL * `method` - HTTP Request Method
- **Key mechanics:** Nostr event, Request Flow, Reference Implementations
- **Event kinds / objects:** 27235
- **Implementation focus (nostr-api):** Store them as replaceable list-style events; keep only the latest per pubkey/kind so stats stay accurate. Update client/relay auth flows accordingly; our API should expose hooks to surface auth requirements or errors to operators. Ensure our HTTP ingest/egress surfaces follow this contract when bridging to relays.

### NIP-99 — Classified Listings
- **Source file:** `99.md`
- **Status:** draft · optional
- **Summary:** - For "tags"/"hashtags" (i.e. categories or keywords of relevance for the listing) the `"t"` event tag should be used. - For images, whether included in the markdown content or not, clients SHOULD use `image` tags as described in [NIP-58](58.md). This allows clients to display images in carousel format more easily.
- **Key mechanics:** Example Event
- **Event kinds / objects:** 30402, 30403
- **Implementation focus (nostr-api):** Treat these as replaceable/addressable records (dedupe per pubkey + kind + optional d-tag) and surface them via filters in the API.

### NIP-A0 — Voice Messages
- **Source file:** `A0.md`
- **Summary:** -   `content`: MUST be a URL pointing directly to an audio file. -   The audio file SHOULD be in `audio/mp4` (.m4a) format using AAC or Opus encoding. Clients MAY support other common audio formats like `audio/ogg`, `audio/webm`, or `audio/mpeg` (mp3), but `audio/mp4` is recommended for broad compatibility and efficiency. -   The audio duration SHOULD be no longer than 60 seconds. Clients publishing `kind: 1222` events SHOULD enforce this limit or provide a clear warning to the user if exceeded. -   `tags`: -   Tags MAY be included as per other NIPs (e.g., `t` for hashtags, `g` for geohash, etc.).
- **Key mechanics:** Specification, Visual representation with `imeta` (NIP-92) tag (optional), Examples
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Primarily client-facing semantics; keep this documented so future features interpret these events/tags consistently.

### NIP-A4 — Public Messages
- **Source file:** `A4.md`
- **Status:** draft · optional
- **Summary:** ```jsonc { "pubkey": "<sender-pubkey>", "kind": 24, "tags": [ ["p", "<receiver>", "<relay-url>"], ], "content": "<message-in-plain-text>", } ```
- **Key mechanics:** Advanced Support, Warnings
- **Event kinds / objects:** 24
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec. Document this behavior in our relay-connection guide so ingestion daemons negotiate capabilities and auth correctly.

### NIP-B0 — Web Bookmarks
- **Source file:** `B0.md`
- **Status:** draft · optional
- **Summary:** * `"t"`, for "tags"/"hashtags" (i.e. topics about which the event might be of relevance) * `"published_at"`, for the timestamp in unix seconds (stringified) of the first time the bookmark was published * `"title"`, title about bookmark and can be used as a attribute for the HTML link element
- **Key mechanics:** Example event
- **Event kinds / objects:** 1111, 39701
- **Implementation focus (nostr-api):** Treat these as replaceable/addressable records (dedupe per pubkey + kind + optional d-tag) and surface them via filters in the API.

### NIP-B7 — Blossom
- **Source file:** `B7.md`
- **Status:** draft · optional
- **Summary:** ```json { "id": "e4bee088334cb5d38cff1616e964369c37b6081be997962ab289d6c671975d71", "pubkey": "781208004e09102d7da3b7345e64fd193cd1bc3fce8fdae6008d77f9cabcd036", "content": "", "kind": 10063, "created_at": 1708774162, "tags": [ ["server", "https://blossom.self.hosted"], ["server", "https://cdn.blossom.cloud"] ], "sig": "cc5efa74f59e80622c77cacf4dd62076bcb7581b45e9acff471e7963a1f4d8b3406adab5ee1ac9673487480e57d20e523428e60ffcc7e7a904ac882cfccfc653" } ```
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** 10063
- **Implementation focus (nostr-api):** Store them as replaceable list-style events; keep only the latest per pubkey/kind so stats stay accurate. Ensure our HTTP ingest/egress surfaces follow this contract when bridging to relays.

### NIP-BE — Nostr BLE Communications Protocol
- **Source file:** `BE.md`
- **Status:** draft · optional
- **Summary:** A device advertises itself with: - Service UUID: `0000180f-0000-1000-8000-00805f9b34fb` - Data: Device UUID in ByteArray format
- **Key mechanics:** Device advertisement, GATT service, Role assignment, Messages
- **Event kinds / objects:** (Not explicitly defined)
- **Implementation focus (nostr-api):** Primarily client-facing semantics; keep this documented so future features interpret these events/tags consistently.

### NIP-C0 — Code Snippets
- **Source file:** `C0.md`
- **Status:** draft · optional
- **Summary:** - `l` - Programming language name (lowercase). Examples: "javascript", "python", "rust" - `name` - Name of the code snippet, commonly a filename. Examples: "hello-world.js", "quick-sort.py" - `extension` - File extension (without the dot). Examples: "js", "py", "rs" - `description` - Brief description of what the code does - `runtime` - Runtime or environment specification (e.g., "node v18.15.0", "python 3.11") - `license` - License under which the code (along with any related data contained within the event, when available, such as the description) is shared. This MUST be a standard [SPDX](https://spdx.org/licenses/) short identifier (e.g., "MIT", "GPL-3.0-or-later", "Apache-2.0") when available. An additional parameter containing a reference to the actual text of the license MAY be provided. This tag can be repeated, to indicate multi-licensing, allowing recipients to use the code under any license of choosing among the referenced ones - `dep` - Dependency required for the code to run (can be repeated) - `repo` - Reference to a repository where this code originates. This MUST be either a standard URL or, alternatively, the address of a [NIP-34](34.md) Git repository announcement event in the form `"30617:<32-bytes hex a pubkey>:<d tag value>"`. If a repository announcement is referenced, a recommended relay URL where to find the event should be provided as an additional parameter
- **Key mechanics:** Abstract, Event Kind, Optional Tags, Format
- **Event kinds / objects:** 1, 1337
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec. Document this behavior in our relay-connection guide so ingestion daemons negotiate capabilities and auth correctly. Ensure our HTTP ingest/egress surfaces follow this contract when bridging to relays.

### NIP-C7 — Chats
- **Source file:** `C7.md`
- **Status:** draft · optional
- **Summary:** ```json { "kind": 9, "content": "GM", "tags": [] } ```
- **Key mechanics:** (Not specified)
- **Event kinds / objects:** 9
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec.

### NIP-EE — E2EE Messaging using MLS Protocol
- **Source file:** `EE.md`
- **Status:** final · unrecommended · optional
- **Upstream note:** **unrecommended**: superseded by the [Marmot Protocol](https://github.com/marmot-protocol/marmot)
- **Summary:** 1. Private _and_ Confidential DMs and Group messages 1. **Private** means that an observer cannot tell that Alice and Bob are talking to one another, or that Alice is part of a specific group. This necessarily requires protecting metadata. 2. **Confidential** means that the contents of conversations can only be viewed by the intended recipients. 2. Forward secrecy and Post-compromise security 1. **Forward secrecy** means that encrypted content in the past remains encrypted even if a key material is leaked. 2. **Post compromise security** means that leaking key material doesn't allow an attacker to continue to read messages indefinitely into the future. 3. Scales efficiently for large groups 4. Allows for the use of multiple device/clients in a single conversation/group.
- **Key mechanics:** Context, Core MLS Concepts, The focus of this NIP, Security Considerations
- **Event kinds / objects:** 445
- **Implementation focus (nostr-api):** Store and index these event types so downstream queries, replay, and analytics behave per spec. Do not mutate ciphertext; just store bytes and index metadata so authorized clients can decrypt off-box. Group/channel events imply additional indexing by shared identifiers; expose convenience filters for operators.
