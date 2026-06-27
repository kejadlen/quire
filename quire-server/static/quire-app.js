function quireApp() {
  return {
    darkMode: false,
    init() {
      const stored = localStorage.getItem('quire-dark');
      if (stored !== null) {
        this.darkMode = stored === '1';
      } else {
        this.darkMode = window.matchMedia('(prefers-color-scheme: dark)').matches;
      }
      this._highlight();
    },
    toggleDark() {
      this.darkMode = !this.darkMode;
      localStorage.setItem('quire-dark', this.darkMode ? '1' : '0');
      this._highlight();
    },
    _highlight() {
      const arborium = window.arborium;
      if (!arborium) return;
      const theme = this.darkMode ? 'github-dark' : 'github-light';
      arborium.highlightAll({ theme });
    }
  }
}
