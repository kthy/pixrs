// PixRS viewer: arrow keys for prev/next, and a notice for videos the browser cannot play.

document.addEventListener("keydown", (event) => {
    if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
    if (event.target.closest("input, textarea, select, video, audio")) return;
    const rel = { ArrowLeft: "prev", ArrowRight: "next" }[event.key];
    const link = rel && document.querySelector(`a[rel="${rel}"]`);
    if (link) link.click();
});

// Media errors do not bubble, so listen in the capture phase.
document.addEventListener(
    "error",
    (event) => {
        if (event.target.tagName !== "VIDEO") return;
        const notice = event.target.parentElement.querySelector(".unplayable");
        if (notice) notice.hidden = false;
    },
    true,
);
