const input = document.querySelector("#search");
const rows = [...document.querySelectorAll("#mapping-table tbody tr")];
const count = document.querySelector("#count");
const buttons = [...document.querySelectorAll("[data-query]")];

function filter() {
  const query = input.value.toLowerCase().trim();
  let matches = 0;
  for (const row of rows) {
    const show = row.textContent.toLowerCase().includes(query);
    row.hidden = !show;
    if (show) matches++;
  }
  count.textContent = `${matches} of ${rows.length} fields`;
  document.querySelector("#empty").hidden = matches > 0;
  for (const button of buttons) {
    const selected = button.dataset.query.toLowerCase() === query;
    button.classList.toggle("active", selected);
    button.setAttribute("aria-pressed", String(selected));
  }
}

input.addEventListener("input", filter);
for (const button of buttons) {
  button.addEventListener("click", () => {
    input.value = button.dataset.query;
    filter();
  });
}
filter();
