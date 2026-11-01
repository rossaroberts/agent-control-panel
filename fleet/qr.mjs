// QR codes for pairing a phone. The encoder is vendored (public/vendor/qrcode.js, qrcode-generator, MIT): one UMD file that
// the browser loads as a script and this module evaluates for Node. The repo's package.json says "type": "module", so
// `require` of that .js file would be read as ESM; running its source with a `module` of our own sidesteps that.
import fs from 'node:fs';

/** The `qrcode(typeNumber, errorCorrectionLevel)` factory from the vendored file. */
export function loadQr() {
  const src = fs.readFileSync(new URL('../public/vendor/qrcode.js', import.meta.url), 'utf8');
  const module = { exports: {} };
  new Function('module', 'exports', src)(module, module.exports);
  return module.exports;
}

/** `text` as a QR code, made at the smallest size that holds it with medium error correction. */
export function encode(text) {
  const qr = loadQr()(0, 'M');
  qr.addData(text);
  qr.make();
  return qr;
}

/** A QR code as terminal lines, two module rows to a line: ▀ top dark, ▄ bottom dark, █ both, space neither. `quiet` modules of
 *  margin on every side. Dark here is the character's foreground, so print it black on white (the caller sets the colours). */
export function qrLines(qr, quiet = 2) {
  const n = qr.getModuleCount();
  const dark = (r, c) => r >= 0 && c >= 0 && r < n && c < n && qr.isDark(r, c);
  const lines = [];
  for (let r = -quiet; r < n + quiet; r += 2) {
    let line = '';
    for (let c = -quiet; c < n + quiet; c++) {
      const top = dark(r, c), bottom = dark(r + 1, c);
      line += top && bottom ? '█' : top ? '▀' : bottom ? '▄' : ' ';
    }
    lines.push(line);
  }
  return lines;
}
