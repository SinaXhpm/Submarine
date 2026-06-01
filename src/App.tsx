import DesktopApp from "./DesktopApp";
import { ConfirmProvider } from "./ui/confirm";

function App() {
  return (
    <ConfirmProvider>
      <DesktopApp />
    </ConfirmProvider>
  );
}

export default App;
