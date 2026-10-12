# Arkiv <> Sourcify

Arkiv Proof of concept: [https://sourcify-poc.vercel.app/](https://sourcify-poc.vercel.app/) password: 123

Repo (temporary): [https://github.com/SantiagoDevRel/argot/tree/master/sourcify-poc](https://github.com/SantiagoDevRel/argot/tree/master/sourcify-poc)

Data Schema: [https://sourcify-data-schema.vercel.app/](https://sourcify-data-schema.vercel.app/) (password: 123)

This is a working document. Nothing here is a commitment until the definition of done and acceptance criteria are signed off on 9th  October. Dates, scope and figures are provisional and will change. Flag anything  for  discussion

Update 24 Aug 2026

## Current State 24 Aug 2026

A proof of concept replicates Sourcify's Unichain dataset onto Arkiv. The framing: Arkiv is a public, queryable mirror of the Sourcify database, not a replacement for the Postgres backend. PoC functionality includes:

* 3,144 Unichain contracts replicated onto our “Cheesecake” devnet in roughly 2,600 transactions.
* Sourcify tables mapped to Arkiv entities: verified contract, compilation, source file, code, signature, and blob. Blobs carry source code that exceeds the transaction byte-size limit, split across entities and rejoined on read.
* `GET /v2/contract/{chainId}/{address}` (including `fields=all`) returns the same body as sourcify.dev for the Unichain snapshot. Listing-with-sort, verify, and stats are out of scope.
* The 4-byte lookup is replicated for the 12,674 selectors that appear in the Unichain snapshot, not Sourcify’s global dictionary.
* Filters the Sourcify API does not expose today: by proxy status, by Solidity compiler version, by function count.
* Every Arkiv node holds the data and its local indexes and answers the query itself. A single query is not distributed across nodes.

## What we think this is useful for, and what we are testing

Our hypothesis is that an Arkiv node holds the full dataset and its indexes and answers queries locally, so a block explorer or wallet can run one and serve itself, no ingest pipeline to maintain and no dependency on your infrastructure or on ours. That is a different thing from the Verifier Alliance, which gives redundancy across verifiers. This gives redundancy in how the same verified data is served.

What is proven and what is not. The mirror works technically at proof of concept scale. Whether anyone currently consuming Sourcify at volume actually wants to run a node is the open question, and it is the main thing this collaboration is designed to find out. Sourcify would potentially be able to scale up query capacity either through Arkiv infrastructure or through infrastructure run by their biggest users.

The second argument, which is proven. The mirror widens what can be asked. The proof of concept already filters by proxy status, compiler version and function count, none of which APIv2 exposes, and any indexed attribute is queryable.

## Proposed deliverable for November milestone:

The proof of concept proved the following:

* The data model maps cleanly between Sourcify's Postgres schema and Arkiv entities.
* The lookup endpoint returns the same body as Sourcify API for the snapshot (24/24 fields)

Note: Arkiv's architecture is changing ahead of the November testnet. The Devcon deliverable will be a fresh build on the new stack rather than a continuation of the current code. We prefer to state this now rather than have it be a surprise in October.

We will deliver a publicly queryable one-off snapshot of Sourcify's Ethereum Mainnet verified-contract database, loaded onto a dedicated Arkiv test network.

### In scope

Chain: Ethereum Mainnet (chain 1)

Cutoff: The mirror contains every contract verified on Sourcify before 30 September 2026. Writing 7.8M addresses will take several days, so the load runs across a window. Contracts verified during the load window may or may not appear. Nothing verified after it will. We publish the cutoff and the load start and end timestamps. The two databases are not synchronized, there is no live listener, and live or incremental sync is explicitly out of scope.

Data: the ten tables Sourcify publishes in its Parquet export, including `compiled_contracts_signatures`, stored as Arkiv entities (verified contract, compilation, source file, code, signature, blob) and composed on read into the same 24 fields `GET /v2/contract/{chainId}/{address}?fields=all` returns.

Target fidelity: byte-identical JSON on 23 of the 24 fields. `proxyResolution` is the exception: Sourcify computes it per request, the mirror freezes it at index time, so it is compared by value rather than by byte. The Unichain proof of concept reached identical field values on 3,136 of 3,144 contracts; reaching byte identity needs two changes we have scoped, JSON key ordering and copying the signatures table.

#### Signatures

The proof of concept derived the signatures field from the ABI instead of copying the `compiled_contracts_signatures` table. It matched on 3,136 of 3,144 contracts and differed on 8, so for the Mainnet load we are copying the table. Everything we need is in your public export, so this needs nothing from you. Measured on a random sample of 130 Mainnet contracts the field averages 2.4 KB, with a sample maximum of 20 KB; we have not bounded the population maximum. Across 7.8M addresses that projects to roughly 19 GB before deduplication and single-digit GB after, modelled on your own compilation deduplication ratio rather than measured. The serving order inside function, event and error is not alphabetical or by selector, but we verified across 3,144 contracts that it follows a single global order consistent with your signatures table ids, so it reproduces from the export without needing a rule from you.

### Out of scope

The following API endpoints will not be supported:

* `POST /v2/verify` — not support to verify smart contract on Arkiv
* `GET /v2/contracts/{chain}` — listing, sort and `afterMatchId` pagination, blocked on Arkiv server-side ordering and on the absence of cursor or offset
* `GET /v2/contract/all-chains/{address}` — the mirror holds one chain, so it would answer a narrower question than yours
* stats / COUNT

Other features not supported:

* Any chain other than Ethereum Mainnet
* Anything verified after the cutoff
* Incremental sync

### Done means all of the following:

1. Our deliverable will have feature parity with the specified endpoints above, verified by a reproducible test suite both sides can run.
2. The replication is a one-off load of everything verified before 30 September 2026, run across several days, with cutoff and load timestamps published.
3. Read latency published at P50 and P95 against the same query set as the Sourcify API, so the comparison is honest in both directions.
4. The replication code and the test suite are open source under MIT License, in the Arkiv GitHub organisation, so any third party can rebuild the mirror and compare it against Sourcify themselves.
5. A written answer on deletion that your team is comfortable defending to a takedown requester.

If we cannot hit all five, we would rather show a smaller scope honestly at Devcon than a demo that overstates what is running.

### Stretch goal

At least two parties other than Arkiv running a node on the testnet.This goal depends on input from your side, so Arkiv cannot deliver it alone and should not claim it as something we control. It is the outcome we most want from this collaboration, and it is the thing that tests the hypothesis in tab 1.

## Requested input from Sourcify

1. Technical Input → Sourcify Query Profile.pdf
    1. Traffic and scraper profile (to know the load an open mirror could absorb):
        1. Who are the users / developers / companies scraping and querying your API?
        2. What type of data are they after?
        3. How is the full database dump distributed today?
2. Introductions to two or three heavy consumers who would run a node.
    1. The ideal candidate is anyone currently scraping Sourcify at volume: Block explorers, wallets doing clear signing, security and audit tooling.
    2. We would like to know if running their own node serves them better or not.
3. Naming.
    1. As part of this work we would like to describe Sourcify as a collaborator publicly, and to have someone from your team speak to why a queryable public mirror is useful. This is not an endorsement of Arkiv as infrastructure and commits you to nothing beyond the testnet.

## What Arkiv will deliver

1. A permissioned Sourcify DB-Chain.
    1. The economics might be shaped specifically around Sourcify and its users.
    2. Writes would be restricted to the replication pipeline, so no third party can inject a contract that Sourcify did not verify.
    3. Reads and node operations would stay open to anyone.
    4. On a dedicated chain we could also offer unlimited entity lifetimes, so nothing expires and nothing needs renewing.
2. One monorepo in the Arkiv org containing POC code, replication pipeline, architecture and rationale, data mappings, test suite and the UI.
3. Written deletion rationale. Arkiv has native delete by entity key, which also removes it from the index, and entities can expire unless renewed. Only the owner can delete or update. Open point: the entity goes, the transaction that wrote it stays in history. We will provide a written explanation for takedown requesters.

### Build

4. We will start with Ethereum mainnet but will rank successive chains depending on Sourcify priority.
5. Measure against your real query distribution, not synthetic benchmarks → Sourcify Query Profile.pdf
6. Payload size. Chunking already handles payloads above the transaction limit and is hash verified. The open question is whether raising the limit is wanted at all, or whether chunking is the better answer.

### Storage and cost optimisation, once we have the real query distribution

7. Attributes versus payload audit. Anything not actually queried moves to the cheaper payload.
8. Runtime bytecode deduplication. You deduplicate sources and compiled bytecode, but runtime bytecode is duplicated because immutables are filled at deployment. Since runtime bytecode is a function of the compiled bytecode, we can show what that saves on the Arkiv side.
9. Source and bytecode compression. Scope and measure alongside the above.

Note: Entity verification is scoped but may not make the November deliverable, meaning how a consumer proves that what they read from the mirror is what Sourcify actually wrote.

## Current Arkiv Limitations

No archival guarantee: entities on Arkiv can be programmed to expire. An entity only survives past its expiry if it is extended before it lapses. If nobody extends it, it is deleted from the latest state. We are not committing to keeping migrated Sourcify data retrievable indefinitely — retention depends on an ongoing extension process that doesn't yet exist in this PoC.

No support for historical queries: entities on Arkiv can be patched and updated, resulting in the previous data, expiration, and attributes being overwritten and not retained. The query API to be built will only expose the current, latest state of an entity and will not support querying prior versions of an entity, such as “what did this entity data / verified contract data looked like at block X or time T?”.

No guarantee for storing large datasets: some of the Sourcify data contains large entries, such as full Solidity source code, large ABIs or compiled EVM bytecode. Such data being very large, it might not fit within the upper limit that an Arkiv’s entity can store. It is not guaranteed that it will be possible to store this data into Arkiv’s entities; as the max transaction payload sizes and the max data sizes for an entity on Arkiv is not fully defined. However a best effort of supporting such data will be provided by using a chunking method to split these large datasets and store it into multiple entities.

Some data might be duplicated: although the goal of the PoC is to avoid de-duplication of the same datasets replicated across multiple entities (e.g: similar solc compiler settings shared across multiple verified contracts), it is not guaranteed that the same data might be duplicated in Arkiv and, as a result, stored across multiple entities. For instance, if the data migration has to be done multiple times, the migration script might not contain a “check-before-write” step and might not check what has already been written to Arkiv from a previous migration run, resulting in potentially duplicated data.

Owner of the entities not defined: at present, it is not defined who will be the owner (the actor who will be able to patch or delete entities) of the entities representing the Sourcify contract data.

## Open questions for both sides

* Does the mirror serve the Sourcify API shape, or its own?
* Given that you already tell requesters you cannot promise erasure once data is published, what does the mirror have to do for a takedown to be defensible in practice?  We will bring our answer on what expiry and deletion do and do not remove.
* Communications: what we say publicly, when, and together or separately.

## Timelines

| Date | What | Owner | Status |
| --- | --- | --- | --- |
| 4 September | Arkiv testnet live | Arkiv | Complete |
| By 4 September | Sourcify inputs: API calls and query samples, query statistics, what is not served today | Kaan and Lea | Complete |
| By 9 October | Arkiv circulates the feature list, definition of done and draft acceptance criteria, one per feature | Jean, Santiago |  |
| By 9 October | Definition of done and acceptance criteria signed off. | Both |  |
| By 9 October | Cutoff date and expected load duration confirmed | Jean, Santiago |  |
| 15 October | Review: lock endpoint set, chain list and deletion standard and priority order for chains two and three | Both |  |
| 15 October | Consumer introductions for the node ask, and Devcon format decision | Both |  |
| By 12 September | Current limitations published in the doc: no archival guarantee, current state only | Jean, Matthias |  |
| End October | Deliverable live: publicly queryable mirror, per the replication mode locked on 7 Sep | Arkiv |  |
| End October | Artefacts published: chain endpoint, MIT monorepo with docs, rationale, data mappings, website, limitations, support commitment | Arkiv |  |
| October | Third parties running nodes, measurement against real usage | Both |  |
| By 23 October | Acceptance criteria formally verified by Sourcify against the live chain | Kaan and Lea |  |
| By 23 October | Go/no-go on Devcon, and decision on whether the September chain is the Devcon chain or a migration is needed | Both |  |
| November, Devcon 8 | Public moment | Both |  |

## Support commitment

Arkiv operates this chain from go-live until 31 May 2027 as a minimum, with [X] days' notice of any change. If Arkiv supersedes the testnet, Arkiv re-runs the initial load onto the successor chain at its own cost and migrates Sourcify's endpoints. Current state only, no archival guarantee. Support is best effort and commercial terms will be revisited when Arkiv mainnet pricing exists.

## After November

Devcon is the first milestone, not the end of the scope. What follows, in order:

1. Ethereum Mainnet, moving from a one-off snapshot to continuous replication. This is a separate piece of work with its own scope and sign-off, not an extension of the November deliverable, and it does not start until the snapshot is accepted.
2. Chains two and three, on your priority order.
3. Growing the operator set beyond the initial two, so the mirror runs as a standing public resource you can point bulk consumers and scrapers at.

Whether the mirror stays a parallel surface or becomes something Sourcify relies on is an open question, and one we think should be answered by how the snapshot performs rather than decided now.

## Devcon 8 Activations

By November, we propose the following activities:

* A joint technical write-up, published during Devcon week, detailing what it costs to replicate a verification database onto a queryable public chain, what it improves, and what it does not solve.
* A side event with the node operators and heavy consumers we have brought in over October.
* A live demo and Sourcify branded expo area at the Arkiv booth.
* An Arkiv <> Sourcify ETHGlobal hackathon bounty (optional)

These are high level ideas and are subject to further discussion.

## Sourcify Grant Request

### Counter proposal to Grant Request: Sourcify Support for the Arkiv Public Mirror Pilot

Prepared by: Arkiv

#### Structure

Total: $35,000, paid in five tranches. A to C are paid on delivery. D is paid on a decision, whichever way it goes. E is paid on the mirror still being in use after acceptance.

| Tranche | Paid when | Sourcify delivers | Share |
| --- | --- | --- | --- |
| A. Inputs and design | On signing, with items 1 and 2 due by 10/10/2026 | 1. Attribute versus payload recommendation for all 24 fields, plus review of the proposed schema (Kaan).<br>2. Co-owned acceptance criteria[a][b]: Sourcify drafts the API-behaviour half, Arkiv drafts the chain half, both sign.<br>3. Traffic and consumer report v2: consumer categories and volumes, how the dump is distributed today, named node candidates, and a written list of anything that must stay out of the public repo[c][d].<br>4. Three to five short conversations arranged with API consumers on which fields they query and what they cannot ask today. Arkiv runs them. | 15% |
| B. Verification | Mirror live, by [23/10/2026] | 5. Sourcify runs the shared acceptance suite[e] against the live chain and publishes the result, pass or fail.<br>6. Written input on the takedown standard Sourcify applies today, so our deletion answer is tested against a real requester.<br>7. Introductions, defined: a warm email with context, a joint call, and the ask stated in it (run a mirror node against the published suite for a defined period). Blockscout first. Sourcify names who it can; Arkiv does the follow up. | 25% |
| C. Operate and show | Devcon week | 8. Sourcify runs one mirror node, live before Devcon, as a named operator. Arkiv supplies the runbook and support. Maintenance clause below.<br>9. Arkiv in Sourcify docs & vice versa. Co-authored technical write-up published, named author, finalised draft by 30/10/2026.<br>10. Named Sourcify speaker at the side event or booth; the mirror shown and marketing materials at both the Arkiv supporter booth and the Argot community booth.<br>11. A quotable statement on why a queryable public mirror is useful. | 30% |
| D. Decide | Devcon week, alongside C | 12. A written yes, no or not-yet on scoping continuous replication, with reasons. If yes, Sourcify's requirements for a change feed or export cadence, so phase two can be scoped in December. | 10% |
| E. Stay | 90 days after acceptance, or on continuous replication being scoped and signed, whichever comes first | 13. Sourcify's node still running.[f][g]<br>14. The mirror listed in Sourcify's documentation as an experimental alternative access path, and announced at launch.<br>Not paid if any of 13 to 14 lapse before the check. | 20% |

#### Maintenance clause on the node

If maintaining the node exceeds 4 hours per month for two consecutive months, Sourcify may stop without forfeiting tranche C, and we treat that as a product finding rather than a failure. In return, Sourcify reports time spent and issues hit once a month. An agent-operated node counts, provided it runs on Sourcify infrastructure with Sourcify as the named operator.

#### Co-marketing

Both sides contribute; activities to be agreed by 17/10/2026. Baseline: joint announcement at acceptance, mutual docs listing (item 9), shared presence at the Arkiv and Argot booths during Devcon week (item 10).

#### What a yes at Devcon means

If the answer to 12 is yes, phase two gets its own scope and its own funding conversation, and the obligations in E roll into it. Nothing in this grant delays continuous replication if both sides want it sooner.

If the answer is no or not-yet, tranche E runs as written and Arkiv operates the snapshot chain to at least 31 May 2027, per the working document.

#### What stays as agreed

A one-off Ethereum Mainnet snapshot, no live sync in this stage. Pricing to be discussed against Arkiv mainnet tokenomics when they exist. MIT licence on everything Arkiv builds.

## Grant Proposal V2

### Why two parts

Part 1 is the public mirror pilot through Devcon and the decision on what comes after. Part 2 is Sourcify operating a mirror node.

We have separated them because the node depends on Arkiv’s operator programme, which is still being finalised, and we would rather give you a measured maintenance figure before you commit than a guess.

### Part 1: Mirror pilot. $25,000, four tranches.

| Tranche | Paid when | Sourcify delivers | Amount |
| --- | --- | --- | --- |
| A. Inputs and design | On signing, items 1 and 2 by 10 October | 1. Attribute versus payload recommendation for all 24 fields, plus review of the proposed schema.<br>2. Co-owned acceptance criteria: Arkiv’s chain-side criteria are attached; Sourcify adds the API-behaviour criteria its suite will assert; both sign.<br>3. Traffic and consumer report v2: consumer categories and volumes, how the dump is distributed today, named node candidates. For Arkiv’s internal use only; nothing from Sourcify’s logs is published, and only an aggregate query profile could be, with Sourcify’s written approval.<br>4. Three to five short conversations arranged with API consumers on which fields they query and what they cannot ask today. Arkiv runs them. | 20% |
| B. Verification | Mirror live, 23 October | 5. Sourcify runs the shared acceptance suite against the live chain and publishes the result, pass or fail.<br>6. Written input on the takedown standard Sourcify applies today, so our deletion answer is tested against a real requester.<br>7. Introductions, defined: a warm email with context, a joint call, and the ask stated in it. Blockscout first. Sourcify names who it can reach; Arkiv does the follow-up. | 30% |
| C. Show | Devcon week, 3 to 6 November | 8. Co-authored technical write-up published, named author, draft by 30 October.<br>9. The mirror listed in Sourcify’s documentation as an experimental alternative access path, and announced at launch.<br>10. Named Sourcify speaker at the side event or booth; the mirror demoed at both the Arkiv and Argot booths.<br>11. A quotable statement on why a queryable public mirror is useful. | 40% |
| D. Decide | Devcon week, alongside C | 12. A decision on continuous replication: if Arkiv has a live-replicating mirror running by Devcon, whether Sourcify adopts it as the mirror’s operating mode after acceptance, and what it needs to see first; if not, whether to scope it for phase two. | 10% |

Co-marketing. Both sides contribute; activities to be agreed by 15 October. Baseline: joint announcement at acceptance, mutual docs listing (item 9), shared presence at the Arkiv booth during Devcon week (item 10). “Collaborator”, not “partner” or “endorsement”; no reference to mainnet timing or GLM.

#### What a yes at Devcon means.

If the answer to 12 is yes, phase two, Sourcify writing to Arkiv directly, gets its own scope and funding. If no or not-yet, Arkiv operates the snapshot chain to at least 31 May 2027, per the working document.

### Part 2: Operating a mirror node. $10,000, two tranches.

Opens when Arkiv delivers the operator runbook and a measured maintenance estimate. Sourcify has 14 days from receiving them to opt in. If Sourcify does not opt in, Part 2 lapses and Part 1 is unaffected.

| Tranche | Paid when | Sourcify delivers | Amount |
| --- | --- | --- | --- |
| Node live | Within 30 days of opting in | 13. Sourcify runs one mirror node on its own infrastructure, from the runbook, as a named operator. Arkiv supplies the runbook and support. | 40% |
| Node retained | 90 days after node live | 14. The node still running, with a short monthly note on time spent and issues hit.<br>15. The mirror still listed in Sourcify’s documentation. These obligations transfer to any successor chain Arkiv stands up; Arkiv migrates the node and the endpoints at its own cost. | 60% |

#### Maintenance clause.

If maintaining the node exceeds eight hours a month for two consecutive months, Sourcify may stop without forfeiting the node-live tranche, and we treat that as a product finding rather than a failure. An agent-operated node counts, provided it runs on Sourcify infrastructure with Sourcify as the named operator.

#### What stays as agreed.

Acceptance is against the one-off Ethereum Mainnet snapshot at the 1 October 2026 cutoff. Arkiv may additionally run a live-replicating mirror from Sourcify’s public export; that is outside this grant’s acceptance criteria and carries no obligation on Sourcify. Free for the duration of the test phase, with anything beyond that discussed against Arkiv mainnet tokenomics when they exist.

## Acceptance criteria (chain side)

These are Arkiv's acceptance criteria for the chain side of the mirror (item 2 of tranche A). They are what we will measure ourselves against.

* Coverage: 100% of Sourcify's verified Ethereum Mainnet contracts in the one-time snapshot, captured on 1 October 2026 at 09:21:28 UTC, are available and queryable on the Sorbet testnet. The exact UTC cutoff and final contract and entity counts are recorded and published. Estimated scale: ~7.8M contracts.
* Initial ownership: entities are owned by a dedicated wallet controlled by the Arkiv team, which operates the mirror during the agreed support period. Sourcify ownership can be explored in a later phase.
* Retention: all migrated entities, including shared dependencies and storage chunks, are created with the maximum lifetime value (`expiresAt = 2^64 − 1`), verified by readback, so they do not expire and are removed only by explicit deletion.
* Data fidelity: for each contract in the snapshot, the Arkiv reader reconstructs the 23 listed Sourcify fields from Arkiv data, including complete source files and bytecode. Signatures are derived at runtime from the ABI rather than stored on-chain; this replaces the earlier plan to copy the signatures table, and the acceptance suite compares the derived field by value. Live proxy resolution is excluded.
* UI: users can search by contract address; filter by contract name, language, compiler and version, verification match, verification date and time range (`verifiedAt`), deployer, deployment block, EVM version and optimiser settings; explore entities and relationships, using real snapshot data on Sorbet.
* Mumbai evaluation: a reproducible benchmark is completed and latency, throughput, errors and costs are documented and published. Acceptance requires delivering the measurements; performance targets remain undefined until testing.

[a]What do we mean by acceptance criteria? I thought this doc page is the acceptance criteria.

[b]We will draft the acceptance criteria i.e the definition of done and then share it with you for your review. It''ll be in the form of a list

1 total reaction

Kaan Uzdogan reacted with 👍 at 06/10/2026, 11:55 UTC

[c]What are we referring to here?

[d]If there is anything you would not like in the public repo. If not, then I am happy to remove this

[e]also, what's the acceptance suite?

[f]This contradicts with the Sourcify node being optional.

[g]Fair to raise. The node is optional in that you can stop under the maintenance clause without forfeiting tranche C, and we treat that as a product finding. Tranche E is different: it is the retention payment, paid for Sourcify still being a user of the mirror 90 days on.
