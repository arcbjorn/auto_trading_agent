// Theme toggle (remembered per browser) and a transcript that follows its newest turn.
(function () {
  var root = document.documentElement;
  try { var saved = localStorage.getItem('theme'); if (saved) root.dataset.theme = saved; } catch (e) {}
  function label() {
    var dark = root.dataset.theme === 'dark' ||
      (!root.dataset.theme && window.matchMedia('(prefers-color-scheme: dark)').matches);
    var b = document.getElementById('theme'); if (b) b.textContent = dark ? 'light' : 'dark';
  }
  document.addEventListener('click', function (e) {
    if (e.target && e.target.id === 'theme') {
      var dark = root.dataset.theme === 'dark' ||
        (!root.dataset.theme && window.matchMedia('(prefers-color-scheme: dark)').matches);
      root.dataset.theme = dark ? 'light' : 'dark';
      try { localStorage.setItem('theme', root.dataset.theme); } catch (err) {}
      label();
    }
  });
  document.addEventListener('htmx:afterSwap', function (e) {
    var t = document.getElementById('transcript');
    if (t && e.target && (e.target === t || t.contains(e.target))) t.scrollTop = t.scrollHeight;
    var box = document.getElementById('message');
    if (box && e.target && e.target.id === 'transcript') { box.value = ''; box.focus(); }
  });
  // A tool name or preset fills the MCP form; a preset also submits it.
  document.addEventListener('click', function (e) {
    var el = e.target.closest ? e.target.closest('[data-tool]') : null;
    if (!el) return;
    var tool = document.getElementById('tool'), args = document.getElementById('args');
    if (!tool || !args) return;
    tool.value = el.getAttribute('data-tool');
    args.value = el.getAttribute('data-args') || '{}';
    if (el.hasAttribute('data-go') && tool.form && window.htmx) htmx.trigger(tool.form, 'submit');
    else if (el.tagName === 'A') e.preventDefault();
  });
  // A canned prompt fills the box and submits the form.
  document.addEventListener('click', function (e) {
    var el = e.target.closest ? e.target.closest('[data-say]') : null;
    if (!el) return;
    var box = document.getElementById('message');
    if (!box) return;
    box.value = el.getAttribute('data-say');
    var form = box.form; if (form && window.htmx) htmx.trigger(form, 'submit');
  });
  label();
  // Tabs: one section at a time; the active one lives in the URL hash so a reload keeps it.
  function showTab(name) {
    var found = false;
    document.querySelectorAll('section.tab').forEach(function (s) {
      var on = s.id === 'tab-' + name; s.hidden = !on; if (on) found = true;
    });
    if (!found) { showTab('engine'); return; }
    document.querySelectorAll('[data-tab]').forEach(function (b) {
      b.classList.toggle('active', b.getAttribute('data-tab') === name);
    });
  }
  document.addEventListener('click', function (e) {
    var b = e.target.closest ? e.target.closest('[data-tab]') : null;
    if (!b) return;
    e.preventDefault();
    var name = b.getAttribute('data-tab');
    if (location.hash !== '#' + name) history.replaceState(null, '', '#' + name);
    showTab(name);
  });
  window.addEventListener('hashchange', function () { showTab((location.hash || '#engine').slice(1)); });
  showTab((location.hash || '#engine').slice(1));
})();
