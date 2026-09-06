// Line permalinks for the file view (#L42, #L10-L20).
//
// The server renders the code surface as a line-number gutter plus one
// blob. After arborium highlights the blob, this script splits it into
// per-line rows: each line gets a stable anchor, wrapped lines keep the
// gutter aligned, and selection is a class instead of geometry.
// Without JS (or before this runs), the gutter numbers carry the ids, so
// #L42 still jumps natively.
(function () {
  "use strict";

  var surface = document.querySelector(".code-surface");
  if (!surface) return;

  var gutter = surface.querySelector(".code-gutter");
  var code = surface.querySelector("pre.code-body code");

  // Parse "#L42" or "#L42-L99" into a normalized inclusive range.
  function parseHash(hash) {
    var m = /^#L(\d+)(?:-L(\d+))?$/.exec(hash);
    if (!m) return null;
    var start = parseInt(m[1], 10);
    var end = m[2] === undefined ? start : parseInt(m[2], 10);
    if (start > end) {
      var t = start;
      start = end;
      end = t;
    }
    return { start: start, end: end };
  }

  // Move child nodes from src into dst until a newline is consumed.
  // Token elements that span the newline are closed in dst and reopened as
  // shallow clones on the next line, preserving highlighting across lines.
  function takeLine(src, dst) {
    while (src.firstChild) {
      var node = src.firstChild;
      if (node.nodeType === Node.TEXT_NODE) {
        var idx = node.data.indexOf("\n");
        if (idx === -1) {
          dst.appendChild(node);
          continue;
        }
        if (idx > 0) {
          node.splitText(idx);
          dst.appendChild(node);
          node = src.firstChild; // remainder, starting with "\n"
        }
        node.deleteData(0, 1);
        if (!node.data) node.remove();
        return true;
      }
      if (node.nodeType !== Node.ELEMENT_NODE) {
        dst.appendChild(node);
        continue;
      }
      var clone = node.cloneNode(false);
      var hit = takeLine(node, clone);
      dst.appendChild(clone);
      if (!hit) node.remove();
      else return true;
    }
    return false;
  }

  function makeRow(n) {
    var row = document.createElement("div");
    row.className = "code-row";
    var num = document.createElement("a");
    num.className = "code-line-num";
    num.id = "L" + n;
    num.href = "#L" + n;
    num.dataset.line = n;
    num.textContent = n;
    var content = document.createElement("span");
    content.className = "code-line-content";
    row.appendChild(num);
    row.appendChild(content);
    return { row: row, content: content };
  }

  var rows = [];
  var anchorLine = null;

  function split() {
    if (!code || !code.textContent) return;
    var lines = document.createElement("div");
    lines.className = "code-lines";
    var current = makeRow(1);
    lines.appendChild(current.row);
    var n = 1;
    while (takeLine(code, current.content)) {
      if (!code.firstChild) break; // blob ended with "\n": no trailing row
      n += 1;
      current = makeRow(n);
      lines.appendChild(current.row);
    }
    if (gutter) {
      lines.style.setProperty("--quire-gutter-w", gutter.offsetWidth + "px");
    }
    rows = Array.prototype.slice.call(lines.children);
    surface.textContent = "";
    surface.appendChild(lines);
    surface.classList.add("code-surface--rows");
    applySelection(true);
  }

  function applySelection(initial) {
    var sel = parseHash(window.location.hash);
    var start = 0;
    var end = -1;
    if (sel && sel.start <= rows.length) {
      start = sel.start;
      end = Math.min(sel.end, rows.length);
    }
    var first = null;
    for (var i = 0; i < rows.length; i++) {
      var selected = i + 1 >= start && i + 1 <= end;
      rows[i].classList.toggle("selected", selected);
      if (selected && !first) first = rows[i];
    }
    if (first) {
      first.scrollIntoView({ block: initial ? "center" : "nearest" });
      document.dispatchEvent(
        new CustomEvent("quire:lines-selected", {
          detail: { start: start, end: end },
        })
      );
    }
  }

  surface.addEventListener("click", function (ev) {
    var num = ev.target.closest(".code-line-num");
    if (!num) return;
    var line = parseInt(num.dataset.line, 10);
    if (ev.shiftKey && anchorLine !== null) {
      ev.preventDefault();
      var a = Math.min(anchorLine, line);
      var b = Math.max(anchorLine, line);
      window.location.hash = a === b ? "#L" + a : "#L" + a + "-L" + b;
    } else {
      anchorLine = line; // plain click: the href sets the hash natively
    }
  });

  window.addEventListener("hashchange", function () {
    applySelection(false);
  });

  function start() {
    if (window.quireHighlightSettled) {
      split();
      return;
    }
    document.addEventListener("quire:highlight-settled", function onSettled() {
      document.removeEventListener("quire:highlight-settled", onSettled);
      split();
    });
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", start);
  } else {
    start();
  }
})();
