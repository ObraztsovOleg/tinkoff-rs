"use strict";
// Applied before the first paint: the viewer's saved choice, otherwise the system setting.
(() => {
  const root = document.documentElement, media = matchMedia("(prefers-color-scheme: dark)");
  let saved = null;
  try { saved = localStorage.getItem("dashboard-theme"); } catch (_) {}
  root.dataset.themeChoice = ["light", "dark"].includes(saved) ? saved : "auto";
  const apply = () => { root.dataset.theme = root.dataset.themeChoice === "auto" ? (media.matches ? "dark" : "light") : root.dataset.themeChoice; };
  apply();
  media.addEventListener("change", apply);
  window.setThemeChoice = choice => {
    root.dataset.themeChoice = choice;
    try { localStorage.setItem("dashboard-theme", choice); } catch (_) {}
    apply();
  };
})();
