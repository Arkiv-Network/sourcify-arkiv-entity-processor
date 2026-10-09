(() => {
  const slides = [...document.querySelectorAll('.slide')];
  const stage = document.querySelector('.stage');
  const previous = document.querySelector('#previous');
  const next = document.querySelector('#next');
  const notes = document.querySelector('#speaker-notes');
  const notesButton = document.querySelector('#toggle-notes');
  let index = 0;
  function resize() {
    stage.style.setProperty('--slide-scale', stage.clientWidth / 1280);
  }
  function render() {
    const match = /^#slide-(\d+)$/.exec(location.hash);
    index = match ? Math.max(0, Math.min(slides.length - 1, Number(match[1]) - 1)) : 0;
    slides.forEach((slide, i) => { slide.hidden = i !== index; slide.setAttribute('aria-hidden', String(i !== index)); });
    previous.disabled = index === 0; next.disabled = index === slides.length - 1;
    document.querySelector('#slide-counter').textContent = `${index + 1} / ${slides.length}`;
    document.querySelector('.progress-track').setAttribute('aria-valuenow', String(index + 1));
    document.querySelector('#progress-fill').style.width = `${(index + 1) / slides.length * 100}%`;
    document.querySelector('#notes-text').textContent = slides[index].querySelector('.speaker-notes-source').content.textContent;
    document.title = `Sourcify · ${index + 1}/${slides.length} · ${slides[index].querySelector('h1').textContent}`;
    resize();
  }
  function go(target) { location.hash = `slide-${Math.max(0, Math.min(slides.length - 1, target)) + 1}`; }
  function toggleNotes() {
    notes.hidden = !notes.hidden;
    notesButton.setAttribute('aria-pressed', String(!notes.hidden));
  }
  previous.addEventListener('click', () => go(index - 1));
  next.addEventListener('click', () => go(index + 1));
  notesButton.addEventListener('click', toggleNotes);
  window.addEventListener('hashchange', render);
  window.addEventListener('resize', resize);
  document.addEventListener('keydown', event => {
    const target = event.target instanceof Element ? event.target : null;
    if (event.altKey || event.ctrlKey || event.metaKey || target?.closest('input,textarea,select,[contenteditable="true"]')) return;
    if (event.key === ' ' && target?.closest('button,a')) return;
    if (['ArrowRight','PageDown',' '].includes(event.key)) { event.preventDefault(); go(index + 1); }
    else if (['ArrowLeft','PageUp'].includes(event.key)) { event.preventDefault(); go(index - 1); }
    else if (event.key === 'Home') { event.preventDefault(); go(0); }
    else if (event.key === 'End') { event.preventDefault(); go(slides.length - 1); }
    else if (event.key.toLowerCase() === 'n') { event.preventDefault(); toggleNotes(); }
  });
  render();
})();
