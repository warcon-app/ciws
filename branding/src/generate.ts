// Generates the CIWS logo set into branding/, the way Warcon's scripts/branding.ts makes Warcon's:
// the same brass plate with two cut corners, the same 5.5-unit square-capped strokes, and the
// wordmark in Barlow Condensed SemiBold (OFL, see OFL.txt) with 6% tracking, traced to outlines
// so the SVGs render the same everywhere. The glyph on the plate is CIWS's own: one line in, a
// node, three lines out.
//
//   cd branding/src && bun install && bun generate.ts
//
// PNGs are rendered with rsvg-convert (librsvg) when it is installed.
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import opentype from 'opentype.js';

const SRC = import.meta.dir;
const OUT = join(SRC, '..');
const PNG = join(OUT, 'png');
const BRASS = '#D4A843';
const INK = '#0F0F12';
const MIST = '#E8E8E8';
const SLATE = '#8D8D96';

const font = opentype.parse(readFileSync(join(SRC, 'BarlowCondensed-SemiBold.ttf')).buffer.slice(0));

// --- mark: a brass plate with two cut corners and the fan-out glyph, 64x64 ----------------------
const PLATE = 'M0 0H48L64 16V64H16L0 48Z';
function fanout(color: string) {
	return `<path d="M6.5 32H27M27 32L39 20H56M27 32H56M27 32L39 44H56" fill="none" stroke="${color}" stroke-width="5.5" stroke-linecap="square" stroke-linejoin="miter"/>
  <circle cx="27" cy="32" r="5.5" fill="${color}"/>`;
}
const markColour = (plate = BRASS, glyph = INK) => `<path d="${PLATE}" fill="${plate}"/>\n  ${fanout(glyph)}`;
/** Single colour: the glyph is punched out of the plate. */
const markMono = (color: string, id: string) =>
	`<mask id="${id}"><rect width="64" height="64" fill="#fff"/>${fanout('#000')}</mask>
  <path d="${PLATE}" fill="${color}" mask="url(#${id})"/>`;

const svg = (w: number, h: number, body: string, title: string) =>
	`<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${w} ${h}" width="${w}" height="${h}" role="img" aria-label="${title}">\n  ${body}\n</svg>\n`;

// --- text, traced to outlines -------------------------------------------------------------------
const n = (v: number) => (Math.round(v * 100) / 100).toString();
function trace(text: string, cap: number, tracking: number) {
	const size = (cap * font.unitsPerEm) / (font.tables.os2.sCapHeight || 700);
	const scale = size / font.unitsPerEm;
	const path = new opentype.Path();
	let pen = 0;
	// Glyph by glyph: font.getPath() can emit a NaN coordinate for some kerned pairs.
	const glyphs = font.stringToGlyphs(text);
	glyphs.forEach((g, i) => {
		const gp = g.getPath(pen, 0, size);
		path.extend(gp.commands.filter((c) => !Object.values(c).some((v) => typeof v === 'number' && Number.isNaN(v))));
		pen += (g.advanceWidth ?? 0) * scale + tracking * size;
		if (i < glyphs.length - 1) pen += font.getKerningValue(g, glyphs[i + 1]) * scale;
	});
	const bb = path.getBoundingBox();
	const d = path.commands
		.map((c) => {
			switch (c.type) {
				case 'M':
				case 'L':
					return `${c.type}${n(c.x)} ${n(c.y)}`;
				case 'Q':
					return `Q${n(c.x1)} ${n(c.y1)} ${n(c.x)} ${n(c.y)}`;
				case 'C':
					return `C${n(c.x1)} ${n(c.y1)} ${n(c.x2)} ${n(c.y2)} ${n(c.x)} ${n(c.y)}`;
				default:
					return 'Z';
			}
		})
		.join('');
	if (d.includes('NaN')) throw new Error(`NaN tracing ${text}`);
	const width = Math.ceil(bb.x2 - bb.x1);
	const at = (color: string, dx: number, baseline: number) =>
		`<path transform="translate(${(dx - bb.x1).toFixed(2)} ${baseline.toFixed(2)})" fill="${color}" d="${d}"/>`;
	return { width, at };
}

const CAP = 40; // cap height in the 64px lockup
const word = trace('CIWS', CAP, 0.06);

// --- lockups ------------------------------------------------------------------------------------
const GAP = 18;
const H = 64;
const baseline = (H + CAP) / 2; // caps vertically centred on the mark
const horizontal = (mark: string, text: string) =>
	svg(64 + GAP + word.width, H, `${mark}\n  ${word.at(text, 64 + GAP, baseline)}`, 'CIWS');
function stacked(mark: string, text: string) {
	const k = 1.6;
	const markSize = 64 * k;
	const w = Math.max(markSize, word.width) + 32;
	const h = markSize + 28 + CAP + 32;
	return svg(
		w,
		h,
		`<g transform="translate(${((w - markSize) / 2).toFixed(2)} 16) scale(${k})">${mark}</g>\n  ${word.at(text, (w - word.width) / 2, 16 + markSize + 28 + CAP)}`,
		'CIWS'
	);
}

// --- social preview, 1280x640, laid out as Warcon's ----------------------------------------------
function socialPreview() {
	const W = 1280;
	const k = 140 / 64; // the lockup 140px high
	const tag = trace('YOUR WARDOGS KILL FEED, SENT EVERYWHERE', 20, 0.12);
	const x = (W - (64 + GAP + word.width) * k) / 2;
	const corner = `fill="none" stroke="${BRASS}" stroke-opacity="0.55" stroke-width="2"`;
	return svg(
		W,
		640,
		`<rect width="${W}" height="640" fill="${INK}"/>
  <path d="M25 145V25H145" ${corner}/>
  <path d="M1135 615H1255V495" ${corner}/>
  <rect y="634" width="${W}" height="6" fill="${BRASS}"/>
  <g transform="translate(${x.toFixed(2)} 215) scale(${k.toFixed(4)})">${markColour()}\n  ${word.at(MIST, 64 + GAP, baseline)}</g>
  ${tag.at(SLATE, (W - tag.width) / 2, 420)}`,
		'CIWS'
	);
}

const files: Record<string, string> = {
	'ciws-mark.svg': svg(64, 64, markColour(), 'CIWS mark'),
	'ciws-mark-mono-black.svg': svg(64, 64, markMono(INK, 'r'), 'CIWS mark'),
	'ciws-mark-mono-white.svg': svg(64, 64, markMono('#FFFFFF', 'r'), 'CIWS mark'),
	'ciws-logo-on-dark.svg': horizontal(markColour(), MIST),
	'ciws-logo-on-light.svg': horizontal(markColour(), INK),
	'ciws-logo-mono-black.svg': horizontal(markMono(INK, 'r'), INK),
	'ciws-logo-mono-white.svg': horizontal(markMono('#FFFFFF', 'r'), '#FFFFFF'),
	'ciws-stacked-on-dark.svg': stacked(markColour(), MIST),
	'ciws-stacked-on-light.svg': stacked(markColour(), INK),
	'ciws-wordmark-on-dark.svg': svg(word.width, CAP + 2, word.at(MIST, 0, CAP), 'CIWS'),
	'ciws-wordmark-on-light.svg': svg(word.width, CAP + 2, word.at(INK, 0, CAP), 'CIWS')
};
for (const [name, body] of Object.entries(files)) writeFileSync(join(OUT, name), body);
console.log(`wrote ${Object.keys(files).length} SVGs to branding/ (wordmark ${word.width}px wide at cap ${CAP})`);

// --- PNGs -----------------------------------------------------------------------------------------
function render(svgText: string, out: string, args: string[]) {
	const r = Bun.spawnSync(['rsvg-convert', ...args, '-o', join(PNG, out)], { stdin: Buffer.from(svgText) });
	if (r.exitCode !== 0) throw new Error(`rsvg-convert failed for ${out}: ${r.stderr.toString()}`);
}
if (!Bun.which('rsvg-convert')) {
	console.log('rsvg-convert not found: PNGs left as they are');
} else {
	mkdirSync(PNG, { recursive: true });
	for (const size of [16, 32, 64, 192, 256, 512, 1024]) {
		render(files['ciws-mark.svg'], `ciws-mark-${size}.png`, ['-w', `${size}`, '-h', `${size}`]);
	}
	render(files['ciws-mark-mono-black.svg'], 'ciws-mark-mono-black-512.png', ['-w', '512', '-h', '512']);
	render(files['ciws-mark-mono-white.svg'], 'ciws-mark-mono-white-512.png', ['-w', '512', '-h', '512']);
	for (const v of ['on-dark', 'on-light', 'mono-black', 'mono-white']) {
		render(files[`ciws-logo-${v}.svg`], `ciws-logo-${v}@2x.png`, ['-z', '2']);
	}
	for (const v of ['on-dark', 'on-light']) {
		render(files[`ciws-stacked-${v}.svg`], `ciws-stacked-${v}.png`, ['-w', '1024']);
	}
	render(socialPreview(), 'ciws-social-preview.png', ['-w', '1280', '-h', '640']);
	console.log('rendered the PNGs into branding/png/');
}
