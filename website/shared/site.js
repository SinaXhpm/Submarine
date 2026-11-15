// The small things the pages do with JavaScript. Every page works without it:
// the menus are <details>, the pictures are links to the full-size file.
(() => {
  "use strict";
  const doc = document;

  // The header gets a line under it once the page is scrolled.
  const head = doc.querySelector(".site-head");
  if (head) {
    const onScroll = () => head.classList.toggle("scrolled", scrollY > 8);
    addEventListener("scroll", onScroll, { passive: true });
    onScroll();
  }

  // Menus: one open at a time; a click elsewhere or Escape closes them.
  const pops = [...doc.querySelectorAll("details.pop")];
  for (const p of pops) {
    p.addEventListener("toggle", () => {
      if (p.open) for (const o of pops) if (o !== p) o.open = false;
    });
  }
  doc.addEventListener("click", (e) => {
    for (const p of pops) if (p.open && !p.contains(e.target)) p.open = false;
  });
  doc.addEventListener("keydown", (e) => {
    if (e.key !== "Escape") return;
    for (const p of pops) {
      if (p.open) { p.open = false; p.querySelector("summary").focus(); }
    }
  });

  // Screenshots open large on the page instead of as a bare file.
  if (typeof HTMLDialogElement === "function") {
    let box, img;
    const open = (a) => {
      if (!box) {
        box = doc.createElement("dialog");
        box.className = "lightbox";
        const close = doc.createElement("button");
        close.type = "button";
        close.textContent = "×";
        close.setAttribute("aria-label", doc.documentElement.dataset.close || "Close");
        close.addEventListener("click", () => box.close());
        img = doc.createElement("img");
        img.decoding = "async";
        box.append(close, img);
        box.addEventListener("click", (e) => { if (e.target === box) box.close(); });
        box.addEventListener("close", () => { img.removeAttribute("src"); });
        doc.body.append(box);
      }
      const small = a.querySelector("img");
      img.alt = small ? small.alt : "";
      img.src = a.href;
      box.showModal();
    };
    doc.addEventListener("click", (e) => {
      const a = e.target.closest(".shot a");
      if (!a || e.ctrlKey || e.metaKey || e.shiftKey || e.button !== 0) return;
      e.preventDefault();
      open(a);
    });
  }

  // Sections below the first screen fade in as they come into view.
  const still = matchMedia("(prefers-reduced-motion: reduce)").matches;
  const items = [...doc.querySelectorAll(".reveal")];
  if (!still && items.length && "IntersectionObserver" in window) {
    const seen = new IntersectionObserver((entries) => {
      for (const en of entries) {
        if (en.isIntersecting) { en.target.classList.remove("pre"); seen.unobserve(en.target); }
      }
    }, { rootMargin: "0px 0px -8% 0px" });
    for (const el of items) {
      if (el.getBoundingClientRect().top > innerHeight) { el.classList.add("pre"); seen.observe(el); }
    }
  }

  // Long pages: the contents list marks the section being read.
  const toc = doc.querySelector(".toc");
  if (toc && "IntersectionObserver" in window) {
    const links = new Map([...toc.querySelectorAll("a[href^='#']")].map((a) => [a.getAttribute("href").slice(1), a]));
    const visible = new Set();
    const mark = () => {
      let id = null;
      for (const s of doc.querySelectorAll(".doc-body > section[id]")) if (visible.has(s.id)) { id = s.id; break; }
      if (!id) return;
      for (const [k, a] of links) a.classList.toggle("active", k === id);
    };
    const watch = new IntersectionObserver((entries) => {
      for (const en of entries) en.isIntersecting ? visible.add(en.target.id) : visible.delete(en.target.id);
      mark();
    }, { rootMargin: "-90px 0px -55% 0px" });
    for (const id of links.keys()) {
      const s = doc.getElementById(id);
      if (s) watch.observe(s);
    }
  }
})();
