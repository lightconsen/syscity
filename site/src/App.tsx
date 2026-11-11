import Nav from "./components/Nav";
import Hero from "./components/Hero";
import Demo from "./components/Demo";
import Download from "./components/Download";
import Features from "./components/Features";
import ActionCognition from "./components/ActionCognition";
import QuickStart from "./components/QuickStart";
import Footer from "./components/Footer";
import { LangProvider } from "./i18n";

export default function App() {
  return (
    <LangProvider>
      <div className="min-h-screen bg-page text-ink">
        <Nav />
        <main>
          <Hero />
          <Demo />
          <Features />
          <ActionCognition />
          <Download />
          <QuickStart />
        </main>
        <Footer />
      </div>
    </LangProvider>
  );
}
