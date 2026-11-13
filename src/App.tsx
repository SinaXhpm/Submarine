import DesktopApp from "./DesktopApp";
import { ConfirmProvider } from "./ui/confirm";
import { BroadcastProvider } from "./ui/broadcast";
import { useT } from "./i18n";
import { FeedbackHost } from "./components/Feedback";

function App() {
  // A language change renders the whole app again (src/i18n).
  useT();
  return (
    <ConfirmProvider>
      <BroadcastProvider>
        <DesktopApp />
        {/* Send feedback: one form for the profile list and Settings. */}
        <FeedbackHost />
      </BroadcastProvider>
    </ConfirmProvider>
  );
}

export default App;
