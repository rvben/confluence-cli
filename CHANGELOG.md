# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

## [0.1.45](https://github.com/rvben/confluence-cli/compare/v0.1.44...v0.1.45) - 2026-11-25

### Fixed

- **sync**: omit emoji from slug folders and identify pull blockers ([87e0a76](https://github.com/rvben/confluence-cli/commit/87e0a763c404fed149a83c905ab9319e2235a3a1))

## [0.1.44](https://github.com/rvben/confluence-cli/compare/v0.1.43...v0.1.44) - 2026-11-24

### Added

- **sync**: preview tree moves and preserve CI state across renames ([32d287f](https://github.com/rvben/confluence-cli/commit/32d287f14477038f6a8e92a7d1f23f08326d6621))
- **sync**: preserve folder names when refreshing pulled trees ([8a19161](https://github.com/rvben/confluence-cli/commit/8a1916117774c78ec3c0f2513ad8b94cca40f174))

## [0.1.43](https://github.com/rvben/confluence-cli/compare/v0.1.42...v0.1.43) - 2026-11-23

### Added

- **sync**: support safe publishing from fresh CI checkouts ([ef39b14](https://github.com/rvben/confluence-cli/commit/ef39b14e0efa498015cce205ae9e8b0b9cf94545))

## [0.1.42](https://github.com/rvben/confluence-cli/compare/v0.1.41...v0.1.42) - 2026-11-23

### Fixed

- **sync**: resolve published links outside the selected tree ([046cc46](https://github.com/rvben/confluence-cli/commit/046cc4674fd8bc04aec542d004aebbf87f3aada2))
- **sync**: resolve links consistently across equivalent paths ([45b4e56](https://github.com/rvben/confluence-cli/commit/45b4e561b17c067de14a50069118bd51af162969))
- **pull**: fence an element that Markdown reads as text of a paragraph ([ff1a698](https://github.com/rvben/confluence-cli/commit/ff1a6985f840a5da4d0ec9b9ae598e1fe658c70b))
- **apply**: read blocks, storage fences and code from one reading of the text ([ffdbd6b](https://github.com/rvben/confluence-cli/commit/ffdbd6b5683a113556381e6c050982e3a53f036f))
- **apply**: write a checkbox in a numbered list as the text of its item ([cf73d3a](https://github.com/rvben/confluence-cli/commit/cf73d3a3473c81aabb0a0d3600b9762978bef830))
- **apply**: keep the plain items of a list that also holds tasks ([1f2ebc0](https://github.com/rvben/confluence-cli/commit/1f2ebc05862fb84f2d24a5b428d3bf9719ba57c9))
- read a front matter value that ends in three dashes ([c3ee0a0](https://github.com/rvben/confluence-cli/commit/c3ee0a05ad3fba28db6d2eadfc9b49f29431f41f))
- **apply**: keep the parameters of a macro whose body is empty ([0b7d42f](https://github.com/rvben/confluence-cli/commit/0b7d42fb65ccfb9e23d45776caa212464e6af591))
- **pull**: keep a processing instruction that stands between blocks ([f7c6816](https://github.com/rvben/confluence-cli/commit/f7c68168ea16af4ae6739759e3bb854748109b3d))
- read a file written with CRLF line endings ([08379a9](https://github.com/rvben/confluence-cli/commit/08379a9f218f866ae21315a78bde31ae2f7d03cb))
- **apply**: keep text between two rules in the body of a page ([d55fefd](https://github.com/rvben/confluence-cli/commit/d55fefd431b9517f191d02c07b2c316b15ba3f9e))
- **sync**: read the parameters of a macro only where the block holds them ([943862d](https://github.com/rvben/confluence-cli/commit/943862ddd65b2fb46f769e7694e974306a47acb2))
- **apply**: keep the recorded hash of the remote body when none is published ([59b7163](https://github.com/rvben/confluence-cli/commit/59b7163897db3c1753e2c25e4bdb25217a36a471))
- **pull**: read the address of an attachment whole ([e612dbb](https://github.com/rvben/confluence-cli/commit/e612dbb4c047fc25719d4a24beb6167f03756872))
- **pull**: leave a link to a page of another site as it is ([65f8aa6](https://github.com/rvben/confluence-cli/commit/65f8aa6c252a7990afbd8647c4c0903bdbe990b7))
- **pull**: read the address of a page whole ([5d3bc77](https://github.com/rvben/confluence-cli/commit/5d3bc777ab2f86eca1fca6a5c4309abff82c10a6))
- **pull**: rewrite a page address only where Markdown reads a link ([33e21ba](https://github.com/rvben/confluence-cli/commit/33e21ba5d32fe38425c475051dc43025bea8893c))
- **convert**: name the macro block that holds markup that does not convert ([743a3a8](https://github.com/rvben/confluence-cli/commit/743a3a828cbe9802df59b52e2b735ff10e9dc187))
- **pull**: name a mention only where a block holds Markdown ([34239cc](https://github.com/rvben/confluence-cli/commit/34239cc8e645a2bf7023935581d2bf54a6b71061))
- **apply**: read a placeholder address only from the href of a link ([1b1f197](https://github.com/rvben/confluence-cli/commit/1b1f197a00a43c969b90b94d4d0fe51482d934dd))
- **pull**: read the line endings in a comment as a parser does ([e0958be](https://github.com/rvben/confluence-cli/commit/e0958be39e8e852d50934d365b9a82de906343d4))
- **pull**: keep a block that only wraps other blocks ([ca626f7](https://github.com/rvben/confluence-cli/commit/ca626f71d4cbc18bd025ebfd06cb0a073d2a3ee7))
- **pull**: keep what a macro block has no place for ([4e156f9](https://github.com/rvben/confluence-cli/commit/4e156f95902279819b68feb5c8d465ab73129786))
- **pull**: keep a processing instruction inside a block ([dd14d6e](https://github.com/rvben/confluence-cli/commit/dd14d6e63d7c1d3785dcfd3207e9936668b9887f))
- **pull**: keep a paragraph whose second line reads as the row under a table header ([45380df](https://github.com/rvben/confluence-cli/commit/45380df38fcbca87297fecf5735ae5ce4db05222))
- **pull**: keep a macro, a layout or an empty task that a quote holds ([db6d26d](https://github.com/rvben/confluence-cli/commit/db6d26d7971ec28419bda52cf21a594a7ff92a6e))
- **pull**: keep the empty lines at the end of a code or noformat macro ([b5ffdcd](https://github.com/rvben/confluence-cli/commit/b5ffdcd3b3b08cd2b5a1f54a48fc4f2e051c67b2))
- **pull**: keep a macro parameter that does not fit a line ([cb5a14f](https://github.com/rvben/confluence-cli/commit/cb5a14f0bb985134c88ba11bd44ca77676923c6f))
- **pull**: keep two layouts that follow each other apart ([7a4425a](https://github.com/rvben/confluence-cli/commit/7a4425a282dcdefd30b4d828db592248a46eab33))
- **apply**: keep a task without a body a task ([98dea37](https://github.com/rvben/confluence-cli/commit/98dea37b8bafd243df5459cae765f80ada6e0e10))
- **pull**: keep preformatted text whose lines read as block syntax ([7418198](https://github.com/rvben/confluence-cli/commit/7418198b78752b6f54058a6606fe9872c0a5e037))
- **pull**: keep text in a layout cell that reads as the start of a cell ([844b90a](https://github.com/rvben/confluence-cli/commit/844b90aa769d06904992b54c9131781c37fb37c0))
- **pull**: fence a code macro or layout with more tildes than its body holds ([970303f](https://github.com/rvben/confluence-cli/commit/970303f34d0aa7cdcc19b79323cae863dcff8da0))
- **pull**: keep a `---` line in the body of a macro without parameters ([ea56268](https://github.com/rvben/confluence-cli/commit/ea56268caaa88ee62be7cba6ab08c7b011ed6860))
- **pull**: keep the title and icon setting of a panel ([5c4c600](https://github.com/rvben/confluence-cli/commit/5c4c600e6ed556a1ed8bfc97835077c8037a7e75))
- **apply**: read macro and layout blocks by their nesting and leave code alone ([febe83c](https://github.com/rvben/confluence-cli/commit/febe83c6e7f025690d8d553e1f3b77a3d5972f8a))
- **apply**: keep the paragraph after a panel, expand or code block apart from it ([9b4340b](https://github.com/rvben/confluence-cli/commit/9b4340b76bd0b03542a1064b834f293a36f5f487))

## [0.1.41](https://github.com/rvben/confluence-cli/compare/v0.1.40...v0.1.41) - 2026-11-20

### Added

- **pull**: show the name of a mentioned user ([a8a247d](https://github.com/rvben/confluence-cli/commit/a8a247d8873e13af55c11c6db84ba4c478329fc0))

### Fixed

- **apply**: keep a closing tag that a comment inside a placeholder link holds ([8651b35](https://github.com/rvben/confluence-cli/commit/8651b3528f6f4406f6f067c233a02ebec28eb48e))
- **pull**: link to the page an address names, not one whose id it starts with ([a6c470b](https://github.com/rvben/confluence-cli/commit/a6c470b5497315814639b22172e1f2c8d571c003))
- **apply**: resolve a placeholder link that follows a comment naming CDATA ([3337c2b](https://github.com/rvben/confluence-cli/commit/3337c2b896358f9a72e091dd52d325d2842dd751))
- **pull**: keep a comment that a page holds ([3bf3db0](https://github.com/rvben/confluence-cli/commit/3bf3db0e18465dde97b996e698349c6f68bb44f4))
- **apply**: keep a block that follows a layout outside it ([ca4fcde](https://github.com/rvben/confluence-cli/commit/ca4fcdebf864e69d21a9284c572a7c9858378403))
- **pull**: keep line breaks and code spans that Markdown reads differently ([9b6c242](https://github.com/rvben/confluence-cli/commit/9b6c242533a2508c85670fbb68b3b7e8d097c935))
- **pull**: keep the formatting of a page that names characters by entity ([7667f97](https://github.com/rvben/confluence-cli/commit/7667f97e0fbe2bb6bff216fa3239010eab03e501))
- **apply**: keep markup characters in the title of a status ([c060586](https://github.com/rvben/confluence-cli/commit/c0605861b11a939e7f22c8e96c555407c5e5cb1d))
- **apply**: leave a placeholder link inside a comment as text ([aff23bd](https://github.com/rvben/confluence-cli/commit/aff23bd0397272d1c6f95c5903cd0fb2e92ef36b))
- **convert**: name the file and Markdown lines of a body that does not convert ([4166f2a](https://github.com/rvben/confluence-cli/commit/4166f2a20b185c9d4a5db8b5407c811baf7bdae6))
- **pull**: write the pages that convert and mark what Markdown does not carry ([e8cd504](https://github.com/rvben/confluence-cli/commit/e8cd504ab767541bd024d48850d127fd44ed88c3))
- **convert**: write placeholder links as apply writes them ([039fbf1](https://github.com/rvben/confluence-cli/commit/039fbf1f13a81c6d8da7e953c8b49c426f0da163))
- **markdown**: keep on pull what Markdown cannot carry ([c5a01a8](https://github.com/rvben/confluence-cli/commit/c5a01a89f2470177ea9b7459f72cc701d5f6a76d))

## [0.1.40](https://github.com/rvben/confluence-cli/compare/v0.1.39...v0.1.40) - 2026-11-20

### Fixed

- **sync**: leave addresses inside raw storage blocks as pulled ([cf33617](https://github.com/rvben/confluence-cli/commit/cf3361797c9c38771677c94bb2599c023cb42c2f))
- **markdown**: keep a link to a page that it does not name ([61b2131](https://github.com/rvben/confluence-cli/commit/61b2131ad6fcbd736f84594ec609b227f3157e47))
- **markdown**: keep how a link is displayed ([0e0d740](https://github.com/rvben/confluence-cli/commit/0e0d7402183282f29e0474efdf68b645ae73118c))
- **markdown**: restore Confluence markup when storage is not well formed ([01fb491](https://github.com/rvben/confluence-cli/commit/01fb4913ee8cd54cc3f385617cf9d1446937580d))
- **markdown**: keep preformatted text that holds a line of backticks ([b15176c](https://github.com/rvben/confluence-cli/commit/b15176cd0b3df2b05b1b8d552f1e0387aa1fd98d))
- **markdown**: keep raw storage blocks whole on apply ([20e8963](https://github.com/rvben/confluence-cli/commit/20e89637587024ce413b1b158fd19f6c7bd34fda))
- **markdown**: escape a leading colon that has no space after it ([73a7c26](https://github.com/rvben/confluence-cli/commit/73a7c2635320dc965735711b25412c9c19261dd9))
- **markdown**: keep the paragraphs of a list item apart ([f2ef33f](https://github.com/rvben/confluence-cli/commit/f2ef33fff197d04ad99445dcd269c21aaaad8a72))

## [0.1.39](https://github.com/rvben/confluence-cli/compare/v0.1.38...v0.1.39) - 2026-11-19

### Fixed

- **markdown**: keep task lists that follow each other separate ([a73bf6c](https://github.com/rvben/confluence-cli/commit/a73bf6c66894dd87f710a83ebd4495a5696cc0ec))
- **markdown**: read version-stamped attachments as Markdown ([063fdbf](https://github.com/rvben/confluence-cli/commit/063fdbf1f82d3b7b29bae20fb3b3862f38de9e28))
- **markdown**: keep indented or commented lists separate ([8fef953](https://github.com/rvben/confluence-cli/commit/8fef953f3fac7fcc43d7bc302cc11b21b0e80f63))

## [0.1.38](https://github.com/rvben/confluence-cli/compare/v0.1.37...v0.1.38) - 2026-11-19

### Fixed

- **markdown**: keep lists that follow each other separate ([9ebd56e](https://github.com/rvben/confluence-cli/commit/9ebd56e250207541cb2f91fcb75da752f24f950c))
- **sync**: keep page content intact across pull and apply ([bda1b93](https://github.com/rvben/confluence-cli/commit/bda1b936b6d51894af66d3a191762753d56588d4))
- **provider**: look a space up directly instead of scanning the first 500 ([bb974a1](https://github.com/rvben/confluence-cli/commit/bb974a1b289f1c4535d4219467d9066f62aeaefd))
- **deps**: resolve security advisories and refresh dependencies ([e2c660c](https://github.com/rvben/confluence-cli/commit/e2c660c466a09b85d1be11b3ed6e160b828645c0))

## [0.1.37](https://github.com/rvben/confluence-cli/compare/v0.1.36...v0.1.37) - 2026-10-06

### Fixed

- Include the bundled macro templates in the Nix source fileset so flake builds can compile the offline `template` command.

## [0.1.36](https://github.com/rvben/confluence-cli/compare/v0.1.35...v0.1.36) - 2026-10-06

### Added

- Offline `validate` command for Markdown and Confluence storage bodies, with stdin support and structured errors carrying line and column.
- Offline `convert` command saves generated storage XML for inspection, retaining the artifact and reporting its path if validation fails. Existing outputs require `--force`, and source files cannot be overwritten.
- Built-in code, expand, and noformat macro templates, plus a bundled Confluence agent skill covering storage authoring, version-safe updates, and bounded write-failure recovery.
- Automatic storage validation before page, blog, and comment body writes, plus checks on generated sync content. Malformed XML, broken CDATA, duplicate macro bodies, and known macro body type mismatches fail locally without submitting the body.

### Fixed

- Invalid or unreadable body files return `invalid_input` (exit 2).
- Non-UTF-8 argument paths no longer panic during error-output selection; conversion rejects unreportable output paths before creating artifacts.

## [0.1.35](https://github.com/rvben/confluence-cli/compare/v0.1.34...v0.1.35) - 2026-09-28

### Fixed

- **deps**: pin serial_test to 3.5 to keep MSRV 1.90 while dropping scc ([7ae275b](https://github.com/rvben/confluence-cli/commit/7ae275b3dc0676cf4446c17a2c59205417c88fdd))

## [0.1.34](https://github.com/rvben/confluence-cli/compare/v0.1.33...v0.1.34) - 2026-09-28

### Fixed

- **deps**: bump serial_test to 4 to drop unsound scc dependency (RUSTSEC-2026-0205) ([3f48701](https://github.com/rvben/confluence-cli/commit/3f487019aaef91da65de850addd59934e6d9e568))

## [0.1.33](https://github.com/rvben/confluence-cli/compare/v0.1.32...v0.1.33) - 2026-09-27

### Fixed

- **deps**: update rustls to 0.23.45 for RUSTSEC-2026-0285 ([bba3916](https://github.com/rvben/confluence-cli/commit/bba3916be96430b9c1f3801eaac6e746b28f6acd))

## [0.1.32](https://github.com/rvben/confluence-cli/compare/v0.1.31...v0.1.32) - 2026-09-24

### Added

- **nix**: add a flake for building and developing with Nix ([95dc526](https://github.com/rvben/confluence-cli/commit/95dc52699eef2b58cfb46d08fff6b209baa35c04))

### Fixed

- **tls**: trust the operating-system certificate store ([76d0728](https://github.com/rvben/confluence-cli/commit/76d0728a7377be3167c42606bb6c61fe8879a34f))

## [0.1.31](https://github.com/rvben/confluence-cli/compare/v0.1.30...v0.1.31) - 2026-09-03

### Added

- **auth**: standardize authentication workflow ([aae7779](https://github.com/rvben/confluence-cli/commit/aae77797c2d2124f4368f2598aec0172b182286f))

## [0.1.30](https://github.com/rvben/confluence-cli/compare/v0.1.29...v0.1.30) - 2026-08-31

### Fixed

- **auth**: smooth guided setup and restore terminal state ([b21817e](https://github.com/rvben/confluence-cli/commit/b21817e4fe98d81525484153d51eb86f2ce9bc76))

## [0.1.29](https://github.com/rvben/confluence-cli/compare/v0.1.28...v0.1.29) - 2026-08-29

### Fixed

- **schema**: conform to clispec v0.3 ([883f55c](https://github.com/rvben/confluence-cli/commit/883f55c149086cfc633e82d2d19f1b794f1b19b2))

## [0.1.28](https://github.com/rvben/confluence-cli/compare/v0.1.27...v0.1.28) - 2026-08-29

### Fixed

- **readme**: replace broken Proof Desk screenshot ([d0ff262](https://github.com/rvben/confluence-cli/commit/d0ff262293f1cf9b8c3868b8bfaabc1e6155df1e))

## [0.1.27](https://github.com/rvben/confluence-cli/compare/v0.1.26...v0.1.27) - 2026-08-29

### Added

- **tui**: add Proof Desk workspace ([b3a8181](https://github.com/rvben/confluence-cli/commit/b3a8181d1cfad5b5b926fa34a914e753ee90035c))

## [0.1.26](https://github.com/rvben/confluence-cli/compare/v0.1.25...v0.1.26) - 2026-08-29

### Fixed

- **release**: tolerate crates.io visibility timeouts ([6c9f764](https://github.com/rvben/confluence-cli/commit/6c9f764c9c70e83c41086ed60ff0de9677824b81))

## [0.1.25](https://github.com/rvben/confluence-cli/compare/v0.1.24...v0.1.25) - 2026-08-29

### Fixed

- **ci**: make Cloud E2E environment-driven ([3c61f71](https://github.com/rvben/confluence-cli/commit/3c61f71e6b64a622568edb27526a5dbbe28da90f))
- **http**: bound stalled Confluence requests ([59d2e3e](https://github.com/rvben/confluence-cli/commit/59d2e3e62f4c27c862e055d266a452b55e17b86b))

## [0.1.24](https://github.com/rvben/confluence-cli/compare/v0.1.23...v0.1.24) - 2026-08-28

### Fixed

- **auth**: support secure credentials in cloud e2e ([7c2798b](https://github.com/rvben/confluence-cli/commit/7c2798b38b713ad3e36a1241a08896c55ccab020))

## [0.1.23](https://github.com/rvben/confluence-cli/compare/v0.1.22...v0.1.23) - 2026-08-28

## [0.1.22](https://github.com/rvben/confluence-cli/compare/v0.1.21...v0.1.22) - 2026-08-28

### Fixed

- prevent silent synchronization drift ([6c54f31](https://github.com/rvben/confluence-cli/commit/6c54f31cd6562cafb0190384408bb3b79b7b5aec))

## [0.1.21](https://github.com/rvben/confluence-cli/compare/v0.1.20...v0.1.21) - 2026-08-28

### Fixed

- harden synchronization and release safety ([4b6624e](https://github.com/rvben/confluence-cli/commit/4b6624ed018b537dc9185bbcb5c4607f61b7d896))

## [0.1.20](https://github.com/rvben/confluence-cli/compare/v0.1.19...v0.1.20) - 2026-08-28

### Added

- Add the read-only Proof Desk TUI for space/page browsing, page evidence, and local sync-plan review.
- Complete human-readable help for every command and public argument.
- Executable CLI contract tests for help, parser errors, sync paths, output modes, and schemas.
- Scheduled PyPI installation smoke tests and Linux ARM release artifacts.

### Changed

- Make pagination, JSON success output, destructive confirmations, and agent schemas consistent.
- Use typed runtime errors instead of classifying failures from English substrings.
- Stage pulls as complete local snapshots, preflight whole-tree applies before writing, and retry only safe read requests.
- Return fully hydrated list and tree content, including labels and properties.

### Fixed

- **cli**: harden human and automation contracts ([1e6303d](https://github.com/rvben/confluence-cli/commit/1e6303d4863fef57d51f6f475c098374dfd4673a))
- Prevent body-writing commands from panicking on a Clap argument-ID collision.
- Reject missing or unreadable sync paths instead of reporting a successful no-op plan.
- Correct E2E binary discovery, auth table columns, schema output fields, and short output flags.
- Refuse dirty pull destinations unless `--force` is explicit, reject unsafe attachment names, and propagate attachment failures.
- Paginate labels, properties, attachments, comments, and `pull space --since` results beyond 200 items.
- Preserve transport error types, avoid replaying mutations, scope searches server-side, and report partial mutations structurally.
- Compute real plan diffs, validate destructive targets before prompting, and remove page-only parenting options from blog commands.
- Harden release ordering, GitHub SSH host verification, Homebrew metadata, and PyPI installation monitoring.

## [0.1.19](https://github.com/rvben/confluence-cli/compare/v0.1.18...v0.1.19) - 2026-08-28

### Added

- **packaging**: add PyPI distribution ([c99efe4](https://github.com/rvben/confluence-cli/commit/c99efe48d484d4d0ac08519301ebe561a0ec58a8))

### Fixed

- **release**: keep Homebrew tap loadable on ARM ([2690190](https://github.com/rvben/confluence-cli/commit/2690190275cfeac9e866e38b1889b0adc742f598))

## [0.1.18](https://github.com/rvben/confluence-cli/compare/v0.1.17...v0.1.18) - 2026-08-26

### Added

- **auth**: unify secure onboarding ([712fa5a](https://github.com/rvben/confluence-cli/commit/712fa5ad001aab3374cc07671511c63ed955f5c4))
- **onboarding**: protect interactive credentials ([080e865](https://github.com/rvben/confluence-cli/commit/080e86511a1d239b08f1f6990aa7d6a8c803f6f5))
- **cli**: unify interaction and safety contract ([3acb608](https://github.com/rvben/confluence-cli/commit/3acb608db0526317d64b04023db31da64f3cfdab))

### Fixed

- **auth**: open personal token page ([1bf46f3](https://github.com/rvben/confluence-cli/commit/1bf46f3d3d4ed587f135ff91d4cfe06b619b9664))
- **ci**: install pinned Rust components ([862f844](https://github.com/rvben/confluence-cli/commit/862f844f4261c49c524c64007ceac2443c873734))

## [0.1.17](https://github.com/rvben/confluence-cli/compare/v0.1.16...v0.1.17) - 2026-08-22

### Fixed

- **cli**: resolve clap output argument collision ([c221a97](https://github.com/rvben/confluence-cli/commit/c221a97df474ddfd30fe9bf810aa36b6dea602e7))

## [0.1.15](https://github.com/rvben/confluence-cli/compare/v0.1.14...v0.1.15) - 2026-06-20

### Added

- **schema**: fill missing output_fields for 14 commands ([9f84a3a](https://github.com/rvben/confluence-cli/commit/9f84a3a05871705f1abb641150eb9016924ab5cb))

## [0.1.14](https://github.com/rvben/confluence-cli/compare/v0.1.13...v0.1.14) - 2026-06-20

## [0.1.13](https://github.com/rvben/confluence-cli/compare/v0.1.12...v0.1.13) - 2026-06-11

### Added

- **clispec**: implement v0.2 compliance, score 28% -> 100% ([1556d16](https://github.com/rvben/confluence-cli/commit/1556d16057f91efb404bb8d3293ff1f11f573026))

### Fixed

- restore --json as hidden alias for --output json ([717eec0](https://github.com/rvben/confluence-cli/commit/717eec02b425cbefc3de81cb0f50b48906d2a871))

## [0.1.12](https://github.com/rvben/confluence-cli/compare/v0.1.11...v0.1.12) - 2026-04-03

## [0.1.11](https://github.com/rvben/confluence-cli/compare/v0.1.10...v0.1.11) - 2026-04-03

### Added

- add schema command, standardize CI, and bump rust-version ([f2cb5d5](https://github.com/rvben/confluence-cli/commit/f2cb5d50a3ddd663ce72f1e224a9184a0a7b77b9))

## [0.1.10](https://github.com/rvben/confluence-cli/compare/v0.1.9...v0.1.10) - 2026-04-02

### Fixed

- **page tree**: paginate through all children in list_children ([362e794](https://github.com/rvben/confluence-cli/commit/362e79458ccccf0a0710cc1585d046c2bfdb2f7f))

## [0.1.9](https://github.com/rvben/confluence-cli/compare/v0.1.8...v0.1.9) - 2026-04-01

### Fixed

- **space list**: paginate through all spaces when --all is passed ([369189e](https://github.com/rvben/confluence-cli/commit/369189eed49e871db5ad52d63a4dab27769b2282))

## [0.1.8](https://github.com/rvben/confluence-cli/compare/v0.1.7...v0.1.8) - 2026-04-01

### Added

- add --all flag and pagination hint to space/page/blog list ([c7574e1](https://github.com/rvben/confluence-cli/commit/c7574e1b0cf4413564d5bdbb00e993ec770de107))

## [0.1.7](https://github.com/rvben/confluence-cli/compare/v0.1.6...v0.1.7) - 2026-04-01

## [0.1.6](https://github.com/rvben/confluence-cli/compare/v0.1.5...v0.1.6) - 2026-04-01

### Added

- install binary as `confluence` instead of `confluence-cli` ([2f98eba](https://github.com/rvben/confluence-cli/commit/2f98eba4f1880e287d95b81852522f5fa54ccdd8))

## [0.1.5](https://github.com/rvben/confluence-cli/compare/v0.1.4...v0.1.5) - 2026-04-01

### Added

- **init**: show token in plain text during setup ([cdafbe0](https://github.com/rvben/confluence-cli/commit/cdafbe0298e7089bb5e98ab8274658233834f7e7))

## [0.1.4](https://github.com/rvben/confluence-cli/compare/v0.1.3...v0.1.4) - 2026-04-01

### Added

- **init**: rewrite interactive setup wizard with custom prompt helpers ([5b85c44](https://github.com/rvben/confluence-cli/commit/5b85c4484e2b70b408a17c58b65416aad0136da3))

## [0.1.3](https://github.com/rvben/confluence-cli/compare/v0.1.2...v0.1.3) - 2026-04-01

### Added

- add interactive init wizard ([b13bfb4](https://github.com/rvben/confluence-cli/commit/b13bfb46f6268cc7ea4d361f7f82a76ad4c4c3bf))

## [0.1.2](https://github.com/rvben/confluence-cli/compare/v0.1.1...v0.1.2) - 2026-03-31

### Added

- add page list, blog list, page move, page copy, comment update, search improvements ([f72c4d1](https://github.com/rvben/confluence-cli/commit/f72c4d1a0448040d5d8340ca27fb195e814e6e7d))

### Fixed

- replace all homebrew version placeholders ([eacd090](https://github.com/rvben/confluence-cli/commit/eacd090766ae11047b71545059a51e3f827ced63))

## [0.1.1](https://github.com/rvben/confluence-cli/compare/v0.1.0...v0.1.1) - 2026-03-31

### Fixed

- generate audit-clean homebrew formula ([4e1894f](https://github.com/rvben/confluence-cli/commit/4e1894f4564fd0bb6b2f83702a527f4db09e7811))

## [0.1.0] - 2026-03-31

First proper public release.

### Added

- Confluence Cloud and Data Center provider support.
- Markdown-first `pull`, `plan`, and `apply` workflow with frontmatter and sidecar metadata.
- Direct CLI commands for spaces, search, pages, blogs, attachments, labels, comments, and properties.
- `doctor` command for profile, auth, connectivity, and local sync-path validation.
- Shell completions and JSON output across the major command surface.
- Local Confluence Data Center Docker setup, backup/restore helpers, and live end-to-end test coverage.
- GitHub Actions CI and tagged release packaging for Linux and macOS.

### Improved

- Confluence-aware Markdown conversion for a broad set of built-in macros, typed resource parameters, attachments, layouts, and page-link cases.
- Remote version drift protection, noop stability after `pull` and `apply`, and safer link reconciliation for local Markdown paths.
- Runtime resiliency with HTTP retry and backoff handling for transient provider failures.
- Release packaging now excludes local Docker backup archives and other non-crate files.

## [0.0.1] - 2026-03-30

Reserved the crate name on crates.io.
