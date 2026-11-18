for (const widget of document.querySelectorAll("[data-tabs]")) {
  const list = widget.querySelector("[data-tab-list]");
  const tabs = Array.from(list.querySelectorAll("button[aria-controls]"));
  const panels = tabs.map((tab) => document.getElementById(tab.getAttribute("aria-controls")));

  if (!tabs.length || panels.some((panel) => !panel || !widget.contains(panel))) {
    continue;
  }

  function showPanel(index) {
    widget.dataset.active = panels[index].id;
    tabs.forEach((tab, position) => {
      const selected = position === index;
      tab.setAttribute("aria-selected", String(selected));
      tab.tabIndex = selected ? 0 : -1;
      panels[position].hidden = !selected;
    });
  }

  list.setAttribute("role", "tablist");
  tabs.forEach((tab, index) => {
    tab.setAttribute("role", "tab");
    panels[index].setAttribute("role", "tabpanel");
    panels[index].setAttribute("aria-labelledby", tab.id);
    panels[index].tabIndex = 0;
    tab.addEventListener("click", () => showPanel(index));
    tab.addEventListener("keydown", (event) => {
      let next;
      switch (event.key) {
        case "ArrowRight":
          next = (index + 1) % tabs.length;
          break;
        case "ArrowLeft":
          next = (index + tabs.length - 1) % tabs.length;
          break;
        case "Home":
          next = 0;
          break;
        case "End":
          next = tabs.length - 1;
          break;
        default:
          return;
      }

      event.preventDefault();
      showPanel(next);
      tabs[next].focus();
    });
  });

  showPanel(0);
  list.hidden = false;
}
