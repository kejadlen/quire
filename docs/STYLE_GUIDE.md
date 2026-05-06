# Quire — Style Guide

> A self-hosted personal source forge. jj-native. Text-first. JavaScript-optional.
> This guide is the canonical reference for any agent producing UI, copy, or
> markup for Quire. It is prescriptive about tokens and vocabulary, looser
> about layout — read it through before you build.

---

## 1. Product personality

Quire is a **gathering of folded leaves, sewn together** — a quire, in
bookbinding, is the smallest unit of a book that can stand on its own. The
product reflects that:

- **One person's tool.** No login, no social graph, no issues, no PRs. You
  push via git or jj; you read via the browser.
- **Text-first.** Reading code and history is the primary activity. Treat
  the page like a printed page, not an app surface.
- **Quiet.** No badges, no gradients, no shadows, no rounded card stacks.
  Hairline rules and whitespace do the structural work.
- **Durable.** Designed to run for years on a small VPS without thinking.
  The UI should feel similarly low-maintenance — no trends, no chrome.
- **jj-native.** jj's vocabulary is the primary vocabulary. Git is the
  interop layer, shown secondarily.

### Voice

- Lowercase prose by default in UI chrome (`quire`, `bookmarks`, `recent
  changes`). Sentence case in body copy and README content.
- Short, plain sentences. No exclamation marks. No emoji.
- Technical when it needs to be — assume the reader knows git and is
  learning jj.

### Anti-patterns (do not do)

- Gradients, glassmorphism, drop shadows, neumorphism.
- Rounded cards with colored left-border accents.
- Emoji or pictographic icon sets. Use unicode glyphs sparingly (`※`, `→`,
  `·`, `—`).
- Fixed sidebars. Columns scroll with the page.
- "Get started" hero blocks, marketing copy, social proof.
- Inventing new accent colors per feature. One accent per palette.

---

## 2. Vocabulary

These terms are load-bearing — use them consistently in copy, labels, and
component names.

| Term | Meaning | UI treatment |
|---|---|---|
| **change** | jj's stable identity across rewrites. 8-char letter id. | First 4 chars in `accent`, last 4 in `muted`. Monospace. |
| **commit** | Underlying git commit-id. 8-char hex. | `mutedFaint`, monospace. Secondary to change-id. |
| **bookmark** | jj's branches. | Pill prefixed with `※`. Label in `accent`. |
| **trunk** | The immutable frontier (below `trunk()` revset). | Hair-rule left rail; `immutable` flag. |
| **conflict** | Tracked on changes with conflicts. | Filled `!` badge in `bad`. |
| **divergent** | Same change-id, different commit-ids. | Split glyph; `bad` color. |
| **(no description set)** | jj's empty description. | Italic, `mutedFaint`. |

Do not write "branch" when you mean bookmark. Do not write "SHA" when you
mean commit-id. Do not write "revision" when you mean change.

---

## 3. Typography

Two families. No exceptions without a reason.

### Families

```css
--font-humanist: "iA Writer Quattro", "iA Writer Quattro V",
                 -apple-system, system-ui, sans-serif;
--font-mono:     "IBM Plex Mono", ui-monospace, monospace;
```

- **Humanist** is the default for body, headings, README, and any prose
  surface.
- **Mono** is for: change-ids, commit-ids, bookmark names, paths, code
  blocks, kbd, top-nav wordmark, section eyebrows, metadata strips, CI
  rows, the footer.

When in doubt: anything that originates from the repo (a name, an id, a
path, a ref) is mono.

### Scale

| Token | Size | Line-height | Used for |
|---|---|---|---|
| `display` | 28px | 1.2 | Page-level h1 in README |
| `h2` | 19px | 1.3 | Section headings in prose |
| `body` | 16px | 1.72 | README and long-form prose |
| `ui` | 15px | 1.6 | Default UI text |
| `meta` | 13px | 1.6 | Top-nav, breadcrumbs |
| `mono-md` | 12.5px | 1.6 | Bookmarks, CI rows, change rows |
| `mono-sm` | 11.5–12px | 1.5 | Author, age, secondary mono |
| `eyebrow` | 11px / `letter-spacing: 1.2px` / uppercase | — | Section labels |
| `kbd` | 10.5px | — | Keyboard hints |

Letter-spacing: `-0.2` to `-0.3` on display-size headings; `+1.2` on
uppercase eyebrows; `+0.2` to `+0.8` on small caps and labels. Default
otherwise.

Headings: `font-weight: 500–600`. Body: 400. Mono is rarely bolded — only
the first 4 chars of a change-id (500) and matched fennel keywords (500).

---

## 4. Color

Quire ships **six palettes**, each with light + dark variants. Every
palette uses the same token shape. Pick one per instance; do not mix.

### Token shape

```ts
type Palette = {
  bg:         string; // page background
  ink:        string; // primary text
  muted:      string; // secondary text
  mutedFaint: string; // tertiary / age / hint
  rule:       string; // primary hairline (between major bands)
  rule2:      string; // secondary hairline (between rows, dotted underlines)
  code:       string; // code-block & inline-code background
  accent:     string; // change-ids, bookmarks, links, code-block left-rail
  ok:         string; // CI pass, ok status
  bad:        string; // CI fail, conflict
};
```

### The six palettes

```ts
// Paper · graphite — the original direction. Warm paper, graphite accent.
PALETTE_PAPER.light = {
  bg:'#f8f4ea', ink:'#1d1a15', muted:'#6b6257', mutedFaint:'#9a9184',
  rule:'#ddd4c1', rule2:'#c7bfae', code:'#efe8d6',
  accent:'#3a3a3a', ok:'#4a7a3a', bad:'#9a3a28',
};
PALETTE_PAPER.dark = {
  bg:'#14120f', ink:'#e8e2d2', muted:'#8a8173', mutedFaint:'#5a5347',
  rule:'#2a2721', rule2:'#1f1d18', code:'#1b1915',
  accent:'#c9c2b0', ok:'#8ab378', bad:'#d47a65',
};

// Bone · ink-red — colder off-white, single inked-red accent.
PALETTE_BONE.light = {
  bg:'#f4f2ec', ink:'#1a1814', muted:'#625a4f', mutedFaint:'#948b7d',
  rule:'#d6cfbe', rule2:'#c0b9a7', code:'#ebe6d4',
  accent:'#a8361d', ok:'#4a7a3a', bad:'#a8361d',
};
PALETTE_BONE.dark = {
  bg:'#141312', ink:'#ece5d4', muted:'#8d8573', mutedFaint:'#5a5347',
  rule:'#2a2822', rule2:'#1f1d18', code:'#1b1a16',
  accent:'#d77560', ok:'#8ab378', bad:'#d77560',
};

// Linen · forest — linen paper, deep forest accent. Quietest palette.
PALETTE_FOREST.light = {
  bg:'#f3efe3', ink:'#1c1a15', muted:'#665e52', mutedFaint:'#958c7e',
  rule:'#d4cdbb', rule2:'#bfb7a4', code:'#eae3d0',
  accent:'#2d5b4a', ok:'#2d5b4a', bad:'#9a3a28',
};
PALETTE_FOREST.dark = {
  bg:'#11130f', ink:'#e6e2d3', muted:'#827a6d', mutedFaint:'#555045',
  rule:'#242722', rule2:'#1c1f1a', code:'#181a15',
  accent:'#7aaa92', ok:'#7aaa92', bad:'#d47a65',
};

// Chalk · cobalt — cooler paper, cobalt ink. Architects' notation.
PALETTE_COBALT.light = {
  bg:'#f5f4ef', ink:'#181a1e', muted:'#5c6168', mutedFaint:'#8d939a',
  rule:'#d5d4cd', rule2:'#bfbeb5', code:'#ebeae3',
  accent:'#1f4f7a', ok:'#4a7a3a', bad:'#9a3a28',
};
PALETTE_COBALT.dark = {
  bg:'#101216', ink:'#e4e6ea', muted:'#838a93', mutedFaint:'#545962',
  rule:'#23262c', rule2:'#1a1d22', code:'#15181d',
  accent:'#7ba4c8', ok:'#8ab378', bad:'#d47a65',
};

// Folio · sepia-gold — strongest folio nod. Warm paper, sepia-gold accent.
PALETTE_SEPIA.light = {
  bg:'#f7eedc', ink:'#1c170f', muted:'#71654f', mutedFaint:'#a09278',
  rule:'#dccfb3', rule2:'#c4b899', code:'#eee1c4',
  accent:'#7a5d2e', ok:'#4a7a3a', bad:'#9a3a28',
};
PALETTE_SEPIA.dark = {
  bg:'#14110b', ink:'#ece1cb', muted:'#8f826b', mutedFaint:'#5d5545',
  rule:'#2a2720', rule2:'#1e1c16', code:'#1b1813',
  accent:'#c4a76f', ok:'#8ab378', bad:'#d47a65',
};

// Ink · dark-native — dark-first; cooler ink with paper-white accent.
PALETTE_INK.light = {
  bg:'#f2f1ec', ink:'#151517', muted:'#5a5a5e', mutedFaint:'#8b8b8f',
  rule:'#d2d1ca', rule2:'#bbbab2', code:'#e9e8e1',
  accent:'#151517', ok:'#4a7a3a', bad:'#9a3a28',
};
PALETTE_INK.dark = {
  bg:'#0f0f12', ink:'#eceae4', muted:'#86858b', mutedFaint:'#545357',
  rule:'#22222a', rule2:'#19191f', code:'#161619',
  accent:'#eceae4', ok:'#8ab378', bad:'#d47a65',
};
```

### Color rules

- **One accent per palette.** Use it for change-id heads, bookmark labels,
  links, and the left-rail of code blocks. Nowhere else.
- All accents are low-chroma so prose still reads quiet. Do not introduce
  saturated hues.
- `ok`/`bad` are reserved for CI status and conflict markers. Do not use
  them as decorative color.
- Hairlines use `rule` between major bands and `rule2` between rows or as
  dotted underlines on links.

---

## 5. Layout

### Page rhythm

- Outer horizontal padding: **56px** on all major bands (`padding: x 56px`).
- Vertical band padding: **22–32px** top, **16–32px** bottom. Tighten for
  metadata strips (10px), expand for prose (32px+).
- Major bands separated by `1px solid rule`.
- Row-level separators (inside lists) use `1px solid rule2` or
  `1px dotted rule2`.

### Two-column

When a page has both a primary reading surface and ancillary metadata
(e.g. README + sidebar stack), use:

```css
display: grid;
grid-template-columns: minmax(0, 2.1fr) minmax(0, 1fr);
column-gap: 0;
border-right between columns: 1px solid rule;
```

Both columns scroll with the page. **No fixed sidebars.** Sidebar content
is `font-mono`, eyebrow + content blocks, separated by 24px.

### Single-column (file/code)

File views go full-width. Use a 2-column grid inside (line-number gutter
+ code body) with `gridTemplateColumns: 'auto 1fr'`. The gutter has
`background: code` and a `1px solid rule2` right border.

### Prose width

README body should max-width around **720px**, even within a wide column.

---

## 6. Components

### Top nav

A single horizontal band:

```
[Q-mark] quire / <repo>            press [?] for shortcuts
```

- Padding: `14px 56px`.
- Border-bottom: `1px solid rule`.
- Wordmark in mono, 14px, weight 500, `letter-spacing: -0.2`.
- Breadcrumb separator: `/` in `rule2`.
- Right-side hint: 11px `mutedFaint`.

### Q-mark logo

16×16 SVG: a 12×12 rounded square (`rx: 1.2`, `stroke: ink`,
`stroke-width: 1.2`) containing three horizontal strokes (lines of text)
and a dashed circle overlay at 35% opacity. Render at `ink` color in any
palette. Do not redraw or restyle.

### Change-id

```jsx
<a><span color="accent" weight={500}>{id.slice(0,4)}</span><span color="muted">{id.slice(4,8)}</span></a>
```

- Mono, 12.5px, no underline.
- Optional `title="commit <hex>"` for hover affordance.

### Commit-id

Mono, `mutedFaint`, 12px. Always shown next to or after a change-id, never
in place of one as primary identity.

### Bookmark

Pill, inline:

```
[ ※ name @remote +ahead -behind ⚠ ]
```

- Border: `1px solid rule2`, `border-radius: 2`. Mono, 10.5px.
- `※` glyph in `mutedFaint` at 9px. Name in `accent`. Remote in
  `mutedFaint`. Ahead/behind in `mutedFaint`, 9.5px. Tracking-broken `⚠`
  in `bad`.

### BookmarkList (sidebar)

Mono 12.5px, line-height 1.8. Each row:

```
[KIND   44px][※ name (dotted-underline)            ][+a −b][age]
```

- KIND uppercase, 10px, `letter-spacing: 0.8`. `trunk` kind in `accent`,
  others in `mutedFaint`.
- Dotted underline on names: `border-bottom: 1px dotted rule2`.
- Numbers tabular: `font-variant-numeric: tabular-nums`.

### CI list

Mono, 12.5px:

```
[● 6px dot][#412][change(4)][age          ][duration]
```

- Dot: 6×6, `border-radius: 3`. `ok` for pass, `bad` for fail.
- Change column shows the first 4 chars of the change-id in `accent`.

### ChangeList row

Grid: `3px 74px 1fr auto auto`, gap 16, padding `6px 56px 6px 53px`.

- 2px left rail (`rule2` if `immutable`, else transparent).
- ChangeId, then description (`ink`), then bookmarks (inline), then
  author, then age.
- Empty description renders as `<NoDesc>` — italic, `mutedFaint`,
  literal text `(no description set)`.
- Conflict flag inserts a `<ConflictMark>` after the description.

### Conflict mark

13×13 filled circle, `background: bad`, white `!` in mono 9px/600.
Optional uppercase `conflict` tag in `bad`, 10px, `letter-spacing: 0.6`.

### Code block

```css
font-family: mono;
font-size: 13px;
line-height: 1.65;
background: code;
color: ink;
padding: 14px 18px;
border-left: 2px solid accent;
overflow: auto;
```

Inline code: same `code` background, `1px 5px` padding, mono 13px.

### Keyboard hint (`kbd`)

```css
font-family: mono;
font-size: 10.5px;
padding: 1px 5px;
border: 1px solid rule2;
border-bottom-width: 2px;
border-radius: 3px;
color: muted;
```

### MetaCol / SideBlock (eyebrow + content)

Eyebrow: mono 11px, `letter-spacing: 1.2`, uppercase, `muted`. 8px below
eyebrow, then 6px to a `1px solid rule2` border-bottom on the eyebrow if
it sits inside a SideBlock. SideBlocks are stacked with `margin-bottom: 24`
(0 on last).

### Footer

`16px 56px 24px`, `border-top: 1px solid rule`. Mono 11px, `mutedFaint`,
`letter-spacing: 0.2`. Left side: version. Right side: `?` shortcut hint.

---

## 7. Glyphs

Use only these. Do not introduce icons.

| Glyph | Use |
|---|---|
| `※` | Bookmark prefix. |
| `→` | Forward pointer ("on main → kxrtpqmn"); also "more" links. |
| `·` | Inline metadata separator (in `rule2`). |
| `/` | Breadcrumb separator (in `rule2`). |
| `—` | Em-dash; submodule placeholders, prose. |
| `!` | Conflict marker (filled circle, white). |
| `⚠` | Tracking-broken bookmark. |

---

## 8. Imagery

There isn't any. Quire renders text on paper-toned backgrounds. If you
must place an image (e.g. a favicon), it is the **folio mark** — the
Q-mark SVG above, exported at 16/32/180.

---

## 9. Accessibility

- Hit-targets in keyboard hints and inline UI may be small visually but
  every primary action has a keyboard shortcut shown in the footer.
- Maintain WCAG AA contrast for `ink` on `bg` and `accent` on `bg` — all
  shipped palettes do. Do not lower contrast for stylistic reasons.
- Links are distinguished by color + dotted underline (`rule2`), not
  color alone.
- Status (CI pass/fail, conflict) uses color + glyph, not color alone.

---

## 10. Quick checklist for any new view

- [ ] Top nav with Q-mark + breadcrumb.
- [ ] One band of identity / metadata under the nav, separated by `rule`.
- [ ] Mono for everything that came from the repo (ids, names, paths).
- [ ] Humanist for prose.
- [ ] One accent color, used only for change-id heads / bookmark labels /
      links / code-block left-rail.
- [ ] 56px horizontal page padding.
- [ ] No shadows, no gradients, no rounded cards.
- [ ] Footer with version + `?` hint.
- [ ] jj vocabulary: change, commit, bookmark, trunk.

---

## 11. Machine-readable token export

```json
{
  "fonts": {
    "humanist": "\"iA Writer Quattro\", \"iA Writer Quattro V\", -apple-system, system-ui, sans-serif",
    "mono": "\"IBM Plex Mono\", ui-monospace, monospace"
  },
  "scale": {
    "display": { "size": 28, "lineHeight": 1.2, "weight": 600, "letterSpacing": -0.3 },
    "h2":      { "size": 19, "lineHeight": 1.3, "weight": 600 },
    "body":    { "size": 16, "lineHeight": 1.72, "weight": 400 },
    "ui":      { "size": 15, "lineHeight": 1.6, "weight": 400 },
    "meta":    { "size": 13, "lineHeight": 1.6, "weight": 400 },
    "monoMd":  { "size": 12.5, "lineHeight": 1.6 },
    "monoSm":  { "size": 11.5, "lineHeight": 1.5 },
    "eyebrow": { "size": 11, "letterSpacing": 1.2, "uppercase": true },
    "kbd":     { "size": 10.5 }
  },
  "spacing": { "page": 56, "band": 22, "section": 32, "row": 6, "block": 24 },
  "rules":   { "major": "1px solid rule", "minor": "1px solid rule2", "dotted": "1px dotted rule2" },
  "palettes": ["PAPER", "BONE", "FOREST", "COBALT", "SEPIA", "INK"],
  "vocab":   ["change", "commit", "bookmark", "trunk", "conflict", "divergent"],
  "glyphs":  { "bookmark": "※", "arrow": "→", "sep": "·", "crumb": "/", "dash": "—", "conflict": "!", "warn": "⚠" }
}
```
