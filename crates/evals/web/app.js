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
})();
