from matplotlib._pylab_helpers import Gcf
from matplotlib.backend_bases import FigureManagerBase
from matplotlib.backends.backend_agg import FigureCanvasAgg


class FigureManager(FigureManagerBase):
    def show(self):
        import sys
        name = self.canvas.get_default_filename()
        self.canvas.figure.savefig(name)
        print('Saved figure to ' + name, file=sys.stderr)

    @classmethod
    def pyplot_show(cls, *, block=None):
        for manager in Gcf.get_all_fig_managers():
            manager.show()
        Gcf.destroy_all()


class FigureCanvas(FigureCanvasAgg):
    manager_class = FigureManager


def _reset():
    import sys
    mpl = sys.modules['matplotlib']
    plt = sys.modules.get('matplotlib.pyplot')
    if plt is not None:
        plt.close('all')
    font_manager = sys.modules.get('matplotlib.font_manager')
    if font_manager is not None:
        font_manager._get_font.cache_clear()
    mathtext = sys.modules.get('matplotlib.mathtext')
    if mathtext is not None:
        mathtext.MathTextParser._parse_cached.cache_clear()
    backend = dict.__getitem__(mpl.rcParams, 'backend')
    dict.update(mpl.rcParams, mpl.rcParamsOrig)
    if plt is not None and backend != mpl.rcParamsOrig['backend']:
        plt.switch_backend(mpl.rcParamsOrig['backend'])
