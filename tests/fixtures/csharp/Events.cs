namespace App
{
    public class Button
    {
        public event System.EventHandler Click;
        public void Press() { Click?.Invoke(this, null); }
    }

    public class Form
    {
        private readonly Button _ok = new Button();
        public Form() { _ok.Click += OnOk; }
        public void OnOk(object s, System.EventArgs e) { }
    }
}
