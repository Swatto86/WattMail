/** @type {import('tailwindcss').Config} */
export default {
  content: ["./index.html", "./src/**/*.{ts,js}"],
  theme: {
    extend: {},
  },
  plugins: [require("daisyui")],
  daisyui: {
    // `business` = dark, `corporate` = light. Matched in index.html's pre-paint
    // script and the Settings theme picker.
    themes: ["business", "corporate"],
    darkTheme: "business",
  },
};
