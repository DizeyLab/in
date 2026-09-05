//! The house replacement for the native `<select>` popup.
//!
//! Every browser engine draws its own `<select>` list, and only Chromium's
//! `::picker(select)` lets that popup be themed — Firefox/WebKit never see
//! it, so styling the native popup is not a real cross-browser option.
//! Instead [`dropdown_script`] progressively enhances any `select.status-select`
//! or `select.field-input` already on the page: the native element stays in
//! the DOM (hidden, so its `name`/value still post the same form field
//! unchanged) while a `<button>` cloning its own classes becomes the
//! visible trigger — same closed look as the select it replaces — and a
//! `position: fixed` panel of option rows, portaled onto `<body>`, stands
//! in for the OS popup.
//!
//! ## The whole box is the hit area
//!
//! The trigger is one child among several inside a control box — a status
//! dot before it, a chevron after it, the box's own padding around all of
//! them — so a button-only listener leaves dead zones exactly where the
//! control looks clickable. That bug has been fixed one control at a time
//! before; this is the version that cannot come back. A delegated listener
//! walks up from whatever was clicked and, on finding the element that
//! *directly* contains a single `.dd-trigger`, opens that trigger's panel.
//!
//! "Directly contains" is the whole of the rule: it pins the hit area to
//! the visual box and no further, so a click on a field's outer row or on
//! the modal around it does not reach in and open something. Nothing has to
//! be marked up for this to work — a new control gets it by existing, which
//! is what makes it structural rather than another remembered convention.
//!
//! Anything else genuinely interactive keeps its own click — a link, a
//! button, a text field. `<label>` is deliberately *not* on that list: the
//! new-task modal wraps its status box in one, and bailing on labels put
//! every dead zone straight back.
//!
//! The delegated click also calls `preventDefault`. A `<label>` forwards a
//! click to the control it labels, so without it the browser delivered a
//! *second* click straight to the trigger, which toggled the panel shut
//! again in the same gesture — opening and closing looked exactly like the
//! dead zone the delegation was added to remove.

use topcoat::Result;
use topcoat::context::Cx;
use topcoat::view::{Unescaped, view};

/// Emitted once per page that renders a `<select>`; the enhancement is
/// per-element and idempotent (`data-dd-done`), re-run on `in:wire` so
/// selects arriving in a soft page swap get theirs too. A select that is
/// already done is not skipped but repaired: the live refresh morphs the
/// server's HTML over the live DOM, and what it writes is the select — its
/// selected option, its option list, its class — while the trigger and panel
/// standing in for it are the client's and are left untouched. So `resync`
/// re-asserts `dd-native`, re-reads the label off the select, and rebuilds
/// the rows when the options themselves changed. Without it the two halves
/// drift: the select unhides itself under its own trigger, or the trigger
/// keeps announcing a status somebody else has already changed. An open
/// panel is left alone — repairing under a pointer is worse than stale. Owns its own
/// `Escape` resolver on `window.__inEsc` (priority 95 — the dropdown
/// outranks the modal chain: Escape on an open panel closes just the
/// panel, never the modal above it. The old per-script listeners made
/// this order path-dependent — a modal swapped in by soft navigation
/// registered its listener last and lost to the dropdown; a full load
/// with `?task=` registered it first and the modal chain won, closing
/// the whole modal over an open dropdown — the registry pins the correct
/// order; the whole table lives in `layout.rs`'s `escape_manager_script`).
pub async fn dropdown_script(cx: &Cx) -> Result {
    const JS: &str = "\
        (function () {\
            if (window.__inDd) { return; }\
            window.__inDd = true;\
            function opts(select) { return Array.prototype.slice.call(select.options); }\
            function closeAll() {\
                document.querySelectorAll('.dd-panel.dd-open').forEach(function (panel) {\
                    panel.classList.remove('dd-open');\
                    if (panel.__ddTrigger) { panel.__ddTrigger.setAttribute('aria-expanded', 'false'); }\
                });\
            }\
            function place(panel, trigger) {\
                var r = trigger.getBoundingClientRect();\
                var h = panel.offsetHeight;\
                var w = panel.offsetWidth;\
                var top = r.bottom + 4;\
                if (top + h > window.innerHeight && r.top - h - 4 >= 0) { top = r.top - h - 4; }\
                top = Math.max(4, Math.min(top, window.innerHeight - h - 4));\
                var left = Math.max(4, Math.min(r.left, window.innerWidth - w - 4));\
                panel.style.left = left + 'px';\
                panel.style.top = top + 'px';\
                panel.style.minWidth = r.width + 'px';\
            }\
            function visibleRows(panel) { return Array.prototype.slice.call(panel.querySelectorAll('.dd-option:not(.dd-option-hidden)')); }\
            function activate(panel, row) {\
                panel.querySelectorAll('.dd-option-active').forEach(function (r) { r.classList.remove('dd-option-active'); });\
                if (!row) { return; }\
                row.classList.add('dd-option-active');\
                if (panel.__ddSearch) { row.scrollIntoView({ block: 'nearest' }); } else { row.focus(); }\
            }\
            function isTypeKey(e) { return e.key && e.key.length === 1 && !e.ctrlKey && !e.altKey && !e.metaKey; }\
            function typeIntoSearch(search, e) {\
                search.focus();\
                search.value += e.key;\
                search.dispatchEvent(new Event('input', { bubbles: true }));\
            }\
            function filterRows(panel, query) {\
                var qTr = query.toLocaleLowerCase('tr');\
                var qEn = query.toLowerCase();\
                panel.querySelectorAll('.dd-option').forEach(function (r) {\
                    var t = r.textContent;\
                    var miss = t.toLocaleLowerCase('tr').indexOf(qTr) === -1 && t.toLowerCase().indexOf(qEn) === -1;\
                    r.classList.toggle('dd-option-hidden', qEn !== '' && miss);\
                });\
                var vis = visibleRows(panel);\
                activate(panel, panel.querySelector('.dd-option-selected:not(.dd-option-hidden)') || vis[0]);\
            }\
            function pick(select, trigger, panel, row) {\
                select.value = row.dataset.value;\
                panel.querySelectorAll('.dd-option').forEach(function (r) {\
                    r.classList.toggle('dd-option-selected', r === row);\
                    r.setAttribute('aria-selected', r === row ? 'true' : 'false');\
                });\
                trigger.textContent = row.textContent;\
                panel.classList.remove('dd-open');\
                trigger.setAttribute('aria-expanded', 'false');\
                trigger.focus();\
                select.dispatchEvent(new Event('change', { bubbles: true }));\
            }\
            function openPanel(select, trigger, panel) {\
                closeAll();\
                var search = panel.__ddSearch;\
                if (search) {\
                    search.value = '';\
                    panel.querySelectorAll('.dd-option').forEach(function (r) { r.classList.remove('dd-option-hidden'); });\
                }\
                panel.classList.add('dd-open');\
                place(panel, trigger);\
                trigger.setAttribute('aria-expanded', 'true');\
                activate(panel, panel.querySelector('.dd-option-selected') || panel.querySelector('.dd-option'));\
                if (search) { search.focus(); }\
            }\
            function fillRows(panel, select) {\
                panel.querySelectorAll('.dd-option').forEach(function (r) { r.remove(); });\
                opts(select).forEach(function (opt) {\
                    var row = document.createElement('button');\
                    row.type = 'button';\
                    row.className = 'dd-option' + (opt.selected ? ' dd-option-selected' : '');\
                    row.textContent = opt.textContent;\
                    row.dataset.value = opt.value;\
                    row.setAttribute('role', 'option');\
                    row.setAttribute('aria-selected', opt.selected ? 'true' : 'false');\
                    panel.appendChild(row);\
                });\
            }\
            function rowsMatch(panel, select) {\
                var rows = panel.querySelectorAll('.dd-option');\
                var all = opts(select);\
                if (rows.length !== all.length) { return false; }\
                for (var i = 0; i < all.length; i++) {\
                    if (rows[i].dataset.value !== all[i].value) { return false; }\
                    if (rows[i].textContent !== all[i].textContent) { return false; }\
                }\
                return true;\
            }\
            function resync(select) {\
                var trigger = select.__ddTrigger, panel = select.__ddPanel;\
                if (!trigger || !panel) { return; }\
                window.__inOwn(select, ['dd-native'], []);\
                if (panel.classList.contains('dd-open')) { return; }\
                if (!rowsMatch(panel, select)) { fillRows(panel, select); }\
                var current = select.options[select.selectedIndex];\
                var label = current ? current.textContent : '';\
                if (trigger.textContent !== label) { trigger.textContent = label; }\
                panel.querySelectorAll('.dd-option').forEach(function (r) {\
                    var on = !!current && r.dataset.value === current.value;\
                    r.classList.toggle('dd-option-selected', on);\
                    r.setAttribute('aria-selected', on ? 'true' : 'false');\
                });\
            }\
            function enhance(select) {\
                if (select.dataset.ddDone) { resync(select); return; }\
                window.__inOwn(select, [], ['data-dd-done']);\
                select.dataset.ddDone = '1';\
                var trigger = document.createElement('button');\
                trigger.type = 'button';\
                trigger.className = select.className + ' dd-trigger';\
                var current = select.options[select.selectedIndex];\
                trigger.textContent = current ? current.textContent : '';\
                trigger.setAttribute('aria-haspopup', 'listbox');\
                trigger.setAttribute('aria-expanded', 'false');\
                window.__inAdded(trigger);\
                select.parentNode.insertBefore(trigger, select);\
                window.__inOwn(select, ['dd-native'], []);\
                /* The form's own submit only serves the no-script path; once \
                   the select is enhanced it duplicates the autosubmit, so it \
                   bows out — directly, not via a stylesheet the skin or a \
                   morph might out-race. */ \
                var go = null; \
                var sibs = select.parentNode.children; \
                for (var si = 0; si < sibs.length; si++) { \
                    if (sibs[si].tagName === 'BUTTON' && sibs[si].type === 'submit' && !sibs[si].classList.contains('dd-trigger')) { go = sibs[si]; break; } \
                } \
                if (go) { window.__inOwn(go, [], ['style']); go.style.display = 'none'; } \
                var panel = document.createElement('div');\
                panel.className = 'dd-panel';\
                panel.setAttribute('role', 'listbox');\
                panel.__ddTrigger = trigger;\
                trigger.__ddPanel = panel;\
                trigger.__ddSelect = select;\
                select.__ddTrigger = trigger;\
                select.__ddPanel = panel;\
                var allOpts = opts(select);\
                var search = null;\
                if ((allOpts.length > 7 && !select.hasAttribute('data-nosearch')) || select.hasAttribute('data-search')) {\
                    search = document.createElement('input');\
                    search.type = 'text';\
                    search.className = 'dd-search';\
                    panel.appendChild(search);\
                    panel.__ddSearch = search;\
                }\
                fillRows(panel, select);\
                window.__inAdded(panel);\
                document.body.appendChild(panel);\
                trigger.addEventListener('click', function (e) {\
                    e.stopPropagation();\
                    if (panel.classList.contains('dd-open')) { closeAll(); } else { openPanel(select, trigger, panel); }\
                });\
                trigger.addEventListener('keydown', function (e) {\
                    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {\
                        e.preventDefault();\
                        if (!panel.classList.contains('dd-open')) { openPanel(select, trigger, panel); return; }\
                        var vis = visibleRows(panel);\
                        var idx = vis.indexOf(panel.querySelector('.dd-option-active')) + (e.key === 'ArrowDown' ? 1 : -1);\
                        if (idx >= 0 && idx < vis.length) { activate(panel, vis[idx]); }\
                    } else if (e.key === 'Enter' && panel.classList.contains('dd-open')) {\
                        e.preventDefault();\
                        var active = panel.querySelector('.dd-option-active');\
                        if (active) { pick(select, trigger, panel, active); }\
                    } else if (search && isTypeKey(e)) {\
                        e.preventDefault();\
                        if (!panel.classList.contains('dd-open')) { openPanel(select, trigger, panel); }\
                        typeIntoSearch(search, e);\
                    }\
                });\
                if (search) {\
                    search.addEventListener('input', function () { filterRows(panel, search.value); });\
                }\
                panel.addEventListener('keydown', function (e) {\
                    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {\
                        e.preventDefault();\
                        var vis = visibleRows(panel);\
                        var idx = vis.indexOf(panel.querySelector('.dd-option-active')) + (e.key === 'ArrowDown' ? 1 : -1);\
                        if (idx >= 0 && idx < vis.length) { activate(panel, vis[idx]); }\
                    } else if (e.key === 'Enter') {\
                        e.preventDefault();\
                        var vis = visibleRows(panel);\
                        var active = panel.querySelector('.dd-option-active') || (vis.length === 1 ? vis[0] : null);\
                        if (active) { pick(select, trigger, panel, active); }\
                    } else if (search && e.target !== search && isTypeKey(e)) {\
                        e.preventDefault();\
                        typeIntoSearch(search, e);\
                    }\
                });\
                panel.addEventListener('click', function (e) {\
                    e.stopPropagation();\
                    var row = e.target.closest('.dd-option');\
                    if (row) { pick(select, trigger, panel, row); }\
                });\
            }\
            window.__inEsc.register(95, function () {\
                var panel = document.querySelector('.dd-panel.dd-open');\
                if (!panel) { return false; }\
                var trigger = panel.__ddTrigger;\
                closeAll();\
                if (trigger) { trigger.focus(); }\
                return true;\
            });\
            document.addEventListener('click', function (e) {\
                var el = e.target;\
                if (!el || !el.closest) { return; }\
                if (el.closest('.dd-panel') || el.closest('.dd-trigger')) { return; }\
                if (el.closest('a, button, input:not([type=hidden]), textarea')) { return; }\
                var box = el.nodeType === 1 ? el : el.parentNode;\
                for (var depth = 0; depth < 3 && box && box.querySelectorAll; depth++) {\
                    var found = box.querySelectorAll('.dd-trigger');\
                    if (found.length === 1 && found[0].parentNode === box && found[0].__ddPanel) {\
                        var t = found[0];\
                        e.stopPropagation();\
                        e.preventDefault();\
                        if (t.__ddPanel.classList.contains('dd-open')) { closeAll(); }\
                        else { openPanel(t.__ddSelect, t, t.__ddPanel); }\
                        return;\
                    }\
                    box = box.parentNode;\
                }\
            }, true);\
            document.addEventListener('click', closeAll);\
            window.addEventListener('scroll', function (e) {\
                var t = e.target;\
                if (t && t.nodeType === 1 && t.classList.contains('dd-panel')) { return; }\
                closeAll();\
            }, true);\
            function enhanceAll() { document.querySelectorAll('select.status-select, select.field-input').forEach(enhance); }\
            enhanceAll();\
            document.addEventListener('in:wire', enhanceAll);\
        })();";
    view! { cx => <script>(Unescaped::new_unchecked(JS))</script> }
}
