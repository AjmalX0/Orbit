// Registers the ORBIT language with mdBook's highlight.js and highlights
// ```orbit blocks (mdBook highlights other languages before this runs).
(function () {
  if (typeof hljs === "undefined") return;
  hljs.registerLanguage("orbit", function (h) {
    return {
      name: "ORBIT",
      keywords: {
        keyword:
          "workflow task schema import step worker using after with when for in max_parallel " +
          "signal query await compute compensated_by " +
          "timeout queue_timeout retry compensate idempotency_key sleep version description input " +
          "output return if then else on",
        literal: "true false null",
        type: "String Int Float Bool Bytes Duration Timestamp UUID JSON List Map",
        built_in:
          "len coalesce is_null string int float contains lower upper trim join keys min max abs " +
          "sum map filter find any all " +
          "execution",
      },
      contains: [
        h.C_LINE_COMMENT_MODE,
        h.COMMENT("/\\*", "\\*/", { contains: ["self"] }),
        h.QUOTE_STRING_MODE,
        { className: "number", begin: "\\b\\d[\\d_]*(\\.\\d[\\d_]*)?(ms|s|m|h|d)?\\b" },
      ],
    };
  });
  document.querySelectorAll("code.language-orbit").forEach(function (block) {
    block.textContent = block.textContent; // drop any earlier markup
    if (hljs.highlightBlock) hljs.highlightBlock(block);
    else hljs.highlightElement(block);
    block.classList.add("hljs");
  });
  // "Home" link back to the landing page, in the left toolbar group.
  var bar = document.querySelector("#menu-bar .left-buttons");
  if (bar && !document.querySelector(".orbit-home")) {
    var a = document.createElement("a");
    a.href = (window.path_to_root || "") + "../";
    a.className = "orbit-home";
    a.textContent = "ORBIT home";
    a.title = "Back to the ORBIT home page";
    bar.appendChild(a);
  }
})();
